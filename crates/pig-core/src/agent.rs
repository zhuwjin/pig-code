//! Subagent profile system.
//! Two built-in profiles + user-level ({data_dir}/agents/*.md) and project-level
//! ({cwd}/.pigcode/agents/*.md) Markdown profiles overriding by name (same system as ZCode):
//! directory precedence is project > user > built-in, and a same-name later entry wholly
//! replaces the earlier one. Project profiles only activate under the session's workspace
//! trust token (`load_profiles_with_trust`). Runtime model overrides live in
//! {data_dir}/agents-state.json.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Default max turns for a subagent (when max_turns is not configured)
pub const DEFAULT_MAX_TURNS: usize = 20;

/// Tools forcibly removed from subagents (prevents nested delegation / plan-mode deadlock / blocking the parent turn with questions)
pub const FORBIDDEN_CHILD_TOOLS: [&str; 5] = [
    "Agent",
    "AgentSwarm",
    "EnterPlanMode",
    "ExitPlanMode",
    "AskUserQuestion",
];

/// Subagent profile
#[derive(Clone, Debug)]
pub struct AgentProfile {
    /// ^[a-zA-Z0-9-]{3,50}$
    pub name: String,
    /// Call rationale shown to the main model
    pub description: String,
    /// None, or containing "*" = all tools
    pub tools: Option<Vec<String>>,
    /// None = inherit the parent session; "providerId/modelId" or bare "modelId" (looked up in the default provider)
    pub model: Option<String>,
    /// Reasoning level name; only effective together with an explicit model
    pub thought_level: Option<String>,
    /// Defaults to DEFAULT_MAX_TURNS
    pub max_turns: Option<usize>,
    /// Whether the system prompt injects AGENTS.md; defaults to true
    pub inject_agents_md: bool,
    /// System prompt body (markdown)
    pub system_prompt: String,
    pub source: AgentSource,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentSource {
    BuiltIn,
    User,
    Project,
}

mod child;
mod parse;
mod profiles;
mod records;
mod store;
mod swarm;

pub(crate) use child::*;
pub(crate) use parse::*;
pub(crate) use profiles::*;
pub(crate) use records::*;
pub use store::set_model_override;
pub(crate) use store::*;
pub(crate) use swarm::*;

/// Find a profile by name: exact match first, then normalized match; multiple hits error listing candidates, zero hits error listing all available names.
pub fn find_profile<'a>(
    profiles: &'a [AgentProfile],
    query: &str,
) -> Result<&'a AgentProfile, String> {
    if let Some(profile) = profiles.iter().find(|p| p.name == query) {
        return Ok(profile);
    }
    let normalized = normalize_name(query);
    let hits: Vec<&AgentProfile> = profiles
        .iter()
        .filter(|p| normalize_name(&p.name) == normalized)
        .collect();
    match hits.len() {
        0 => Err(format!(
            "No subagent found for \"{query}\". Available: {}",
            profiles
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
        1 => Ok(hits[0]),
        _ => Err(format!(
            "\"{query}\" matches multiple subagents: {}",
            hits.iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// Effective tool set for a subagent: profile.tools=None or containing "*" -> all - FORBIDDEN;
/// an explicit list -> list ∩ all - FORBIDDEN (names in the list that do not exist are
/// ignored); a model with neither image nor PDF input additionally removes ReadMediaFile.
/// TodoList is a normal child tool: each child gets its own todo handle, so it can never
/// clobber the parent list.
pub fn child_tool_set(
    profile: &AgentProfile,
    all_tool_names: &[String],
    media: crate::tool::MediaCapabilities,
) -> Vec<String> {
    let base: Vec<&String> = match &profile.tools {
        Some(list) if !list.iter().any(|t| t == "*") => {
            all_tool_names.iter().filter(|n| list.contains(n)).collect()
        }
        _ => all_tool_names.iter().collect(),
    };
    base.into_iter()
        .filter(|name| !FORBIDDEN_CHILD_TOOLS.contains(&name.as_str()))
        .filter(|name| media.any() || name.as_str() != "ReadMediaFile")
        .cloned()
        .collect()
}

/// Reject ambiguous child tool sets instead of silently routing a call to the
/// first name match. MCP names are sanitized/truncated (`mcp__<server>__<tool>`),
/// so two distinct originals can collapse to one name; the child could not
/// address either one deterministically.
pub(crate) fn validate_child_tool_names(
    tools: &[Box<dyn crate::tool::Tool>],
) -> Result<(), String> {
    let mut seen = std::collections::HashSet::new();
    for tool in tools {
        if !seen.insert(tool.name()) {
            let name = tool.name();
            return Err(if name.starts_with("mcp__") {
                format!(
                    "MCP tool name collision: \"{name}\" is produced by more than one connected \
                     MCP server/tool after name sanitization; rename the conflicting server or \
                     tool so each call can be routed deterministically"
                )
            } else {
                format!("Duplicate child tool name: \"{name}\"")
            });
        }
    }
    Ok(())
}

/// Subagent runtime-tool inheritance check: if the narrowed built-in tool set contains write tools
/// (Write/Edit) -> inherit all connected MCP/plugin tools; otherwise (a read-only profile such as
/// explore) inherit only tools marked read-only. Note Bash is not part of the check:
/// explore includes Bash after narrowing yet is a read-only profile — the write-capability
/// check only looks at pure file-change tools (Bash calls themselves still pass the approval
/// gate / rule gate). The input is the narrowed built-in tool names from child_tool_set (MCP
/// tools are not among them; the mcp__ prefix never collides by name).
pub fn child_inherits_all_mcp(child_tool_names: &[String]) -> bool {
    child_tool_names
        .iter()
        .any(|name| name == "Write" || name == "Edit")
}

/// Profile listing for the Agent tool description, one per line.
pub fn agent_description_list(profiles: &[AgentProfile]) -> String {
    profiles
        .iter()
        .map(|p| {
            let tools = match &p.tools {
                None => "all".to_string(),
                Some(list) if list.iter().any(|t| t == "*") => "all".to_string(),
                Some(list) if list.is_empty() => "none".to_string(),
                Some(list) => list.join(", "),
            };
            format!("- {}: {} (tools: {})", p.name, p.description, tools)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test-only trusted listing: the profiles a trusted workspace activates.
    fn load_profiles(cwd: &Path, data_dir: &Path) -> Vec<AgentProfile> {
        load_profiles_with_trust(
            cwd,
            data_dir,
            &crate::config::WorkspaceTrust::new(cwd, /*trusted*/ true),
        )
    }

    // ---------- test helpers ----------

    /// Temp dir: pid + nanos for uniqueness, auto-cleaned on Drop
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let path = std::env::temp_dir().join(format!(
                "pig-agent-test-{tag}-{}-{nanos}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("create temp dir");
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Write one profile file under dir (creates the directory)
    fn write_agent(dir: &Path, file: &str, content: &str) {
        std::fs::create_dir_all(dir).expect("create agents dir");
        std::fs::write(dir.join(file), content).expect("write profile file");
    }

    /// Build a profile varying only model/thought_level (other fields don't matter to this test group)
    fn subagent(model: Option<&str>, level: Option<&str>) -> AgentProfile {
        AgentProfile {
            name: "tester".into(),
            description: "d".into(),
            tools: None,
            model: model.map(str::to_string),
            thought_level: level.map(str::to_string),
            max_turns: None,
            inject_agents_md: true,
            system_prompt: "body text.".into(),
            source: AgentSource::BuiltIn,
        }
    }

    // ---------- frontmatter parsing ----------

    #[test]
    fn parse_full_frontmatter() {
        let md = r#"---
name: code-review
description: "Review code changes"
tools:
  - Read
  - Grep
  - "ExecCommand"
model: deepseek/deepseek-chat
thoughtLevel: high
maxTurns: 30
injectAgentsMd: false
unknown: ignore me
---

You are a code reviewer; output only the issue list.
"#;
        let p = parse_agent_markdown(md).expect("full frontmatter should parse");
        assert_eq!(p.name, "code-review");
        assert_eq!(
            p.description, "Review code changes",
            "surrounding quotes stripped"
        );
        assert_eq!(
            p.tools,
            Some(vec![
                "Read".to_string(),
                "Grep".to_string(),
                "ExecCommand".to_string()
            ])
        );
        assert_eq!(p.model.as_deref(), Some("deepseek/deepseek-chat"));
        assert_eq!(p.thought_level.as_deref(), Some("high"));
        assert_eq!(p.max_turns, Some(30));
        assert!(!p.inject_agents_md);
        assert_eq!(
            p.system_prompt,
            "You are a code reviewer; output only the issue list."
        );
    }

    #[test]
    fn parse_minimal_frontmatter() {
        // Only required fields; the rest take defaults
        let md = "---\nname: helper\ndescription: simple helper\n---\nDo what needs to be done.";
        let p = parse_agent_markdown(md).expect("minimal frontmatter");
        assert_eq!(p.tools, None, "tools default = all");
        assert_eq!(p.model, None, "model default = inherit");
        assert_eq!(p.thought_level, None);
        assert_eq!(p.max_turns, None);
        assert!(p.inject_agents_md, "inject_agents_md defaults to true");
        assert_eq!(p.system_prompt, "Do what needs to be done.");
    }

    #[test]
    fn parse_inline_list() {
        let md = "---\nname: helper\ndescription: d\ntools: [Read, Grep, \"ExecCommand\"]\n---\nbody text.";
        let p = parse_agent_markdown(md).expect("inline list");
        assert_eq!(
            p.tools,
            Some(vec![
                "Read".to_string(),
                "Grep".to_string(),
                "ExecCommand".to_string()
            ])
        );
    }

    #[test]
    fn parse_dash_list_with_comments() {
        // Indented `- item` list + # comment lines + single-quote stripping
        let md = "---\n# this is a comment\nname: helper\ndescription: d\ntools:\n  - Read  \n  - 'Glob'\n---\nbody text.";
        let p = parse_agent_markdown(md).expect("dash list");
        assert_eq!(p.tools, Some(vec!["Read".to_string(), "Glob".to_string()]));
    }

    #[test]
    fn parse_unknown_fields_ignored() {
        let md = "---\nname: helper\ndescription: d\nfoo: bar\nzzz:\n  - a\n---\nbody text.";
        let p = parse_agent_markdown(md).expect("unknown fields should be ignored");
        assert_eq!(p.name, "helper");
        assert_eq!(p.tools, None, "unknown-field list must not leak into tools");
    }

    #[test]
    fn parse_missing_name_err() {
        let md = "---\ndescription: d\n---\nbody text.";
        assert!(parse_agent_markdown(md).unwrap_err().contains("name"));
    }

    #[test]
    fn parse_missing_description_err() {
        let md = "---\nname: helper\n---\nbody text.";
        assert!(
            parse_agent_markdown(md)
                .unwrap_err()
                .contains("description")
        );
    }

    #[test]
    fn parse_invalid_name_err() {
        let too_long = "x".repeat(51);
        for bad in ["ab", "has space", "under_score", too_long.as_str()] {
            let md = format!("---\nname: {bad}\ndescription: d\n---\nbody text.");
            assert!(
                parse_agent_markdown(&md).is_err(),
                "name {bad:?} should be invalid"
            );
        }
    }

    #[test]
    fn parse_bad_max_turns_err() {
        for bad in ["0", "-1", "abc"] {
            let md = format!("---\nname: helper\ndescription: d\nmaxTurns: {bad}\n---\nbody text.");
            assert!(
                parse_agent_markdown(&md).is_err(),
                "maxTurns={bad} should error"
            );
        }
    }

    #[test]
    fn parse_model_inherit_is_none() {
        for value in ["inherit", "main", ""] {
            let md = format!("---\nname: helper\ndescription: d\nmodel: {value}\n---\nbody text.");
            let p = parse_agent_markdown(&md).expect("inherit/main/empty should parse as inherit");
            assert_eq!(p.model, None, "model: {value:?} should be None");
        }
    }

    #[test]
    fn parse_no_frontmatter_err() {
        assert!(parse_agent_markdown("body text without frontmatter").is_err());
        assert!(parse_agent_markdown("").is_err());
    }

    // ---------- load_profiles ----------

    #[test]
    fn load_profiles_override_and_sort() {
        let tmp = TempDir::new("load");
        let data_dir = tmp.0.join("data");
        let cwd = tmp.0.join("proj");
        // user level overrides builtin explore
        write_agent(
            &data_dir.join("agents"),
            "explore.md",
            "---\nname: explore\ndescription: user explore\n---\nuser body text.",
        );
        // project level overrides user level again (precedence: project > user > builtin)
        write_agent(
            &cwd.join(".pigcode/agents"),
            "explore.md",
            "---\nname: explore\ndescription: project explore\n---\nproject body text.",
        );
        // new user-level profile (name sorts last; also verifies ordering)
        write_agent(
            &data_dir.join("agents"),
            "helper.md",
            "---\nname: z-helper\ndescription: user helper\n---\nbody text.",
        );
        // bad file: parse failure should be skipped without affecting other profiles
        write_agent(&data_dir.join("agents"), "bad.md", "not a profile");
        // non-.md files must be ignored
        write_agent(
            &data_dir.join("agents"),
            "note.txt",
            "---\nname: ghost\ndescription: x\n---\nbody text.",
        );

        let profiles = load_profiles(&cwd, &data_dir);
        let names: Vec<&str> = profiles.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            names,
            ["explore", "general-purpose", "z-helper"],
            "sorted by name; bad/non-md files excluded"
        );
        let explore = find_profile(&profiles, "explore").expect("explore");
        assert_eq!(
            explore.description, "project explore",
            "project level overrides user level and builtin"
        );
        assert_eq!(explore.source, AgentSource::Project);
        assert_eq!(
            explore.system_prompt, "project body text.",
            "same name replaces wholesale (body included)"
        );
        let helper = find_profile(&profiles, "z-helper").expect("z-helper");
        assert_eq!(helper.source, AgentSource::User);
        let general = find_profile(&profiles, "general-purpose").expect("builtin general-purpose");
        assert_eq!(general.source, AgentSource::BuiltIn);
    }

    #[test]
    fn load_profiles_user_overrides_builtin() {
        let tmp = TempDir::new("load-user");
        let data_dir = tmp.0.join("data");
        let cwd = tmp.0.join("proj"); // no project-level directory
        write_agent(
            &data_dir.join("agents"),
            "explore.md",
            "---\nname: explore\ndescription: user explore\n---\nbody text.",
        );
        let profiles = load_profiles(&cwd, &data_dir);
        let explore = find_profile(&profiles, "explore").expect("explore");
        assert_eq!(
            explore.description, "user explore",
            "user level overrides builtin"
        );
        assert_eq!(explore.source, AgentSource::User);
    }

    #[test]
    fn load_profiles_missing_dirs_ok() {
        let tmp = TempDir::new("load-empty");
        let profiles = load_profiles(&tmp.0.join("nope"), &tmp.0.join("nope-data"));
        assert_eq!(profiles.len(), 2, "missing dirs = only two builtins remain");
    }

    // ---------- agents-state.json overrides ----------

    #[test]
    fn state_override_applies_to_builtin() {
        let tmp = TempDir::new("state");
        let data_dir = tmp.0.join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::write(
            data_dir.join("agents-state.json"),
            r#"{"model_overrides": {"general-purpose": {"model": "p1/m1", "thought_level": "low"}}}"#,
        )
        .unwrap();
        let profiles = load_profiles(&tmp.0.join("proj"), &data_dir);
        let general = find_profile(&profiles, "general-purpose").expect("general-purpose");
        assert_eq!(
            general.model.as_deref(),
            Some("p1/m1"),
            "builtins have no frontmatter, state must apply"
        );
        assert_eq!(general.thought_level.as_deref(), Some("low"));
        let explore = find_profile(&profiles, "explore").expect("explore");
        assert_eq!(
            explore.model, None,
            "builtins not listed in state unaffected"
        );
    }

    #[test]
    fn state_override_skipped_when_frontmatter_has_model() {
        let tmp = TempDir::new("state-explicit");
        let data_dir = tmp.0.join("data");
        write_agent(
            &data_dir.join("agents"),
            "custom.md",
            "---\nname: custom\ndescription: d\nmodel: p2/m2\n---\nbody text.",
        );
        std::fs::write(
            data_dir.join("agents-state.json"),
            r#"{"model_overrides": {"custom": {"model": "p9/m9", "thought_level": "max"}}}"#,
        )
        .unwrap();
        let profiles = load_profiles(&tmp.0.join("proj"), &data_dir);
        let custom = find_profile(&profiles, "custom").expect("custom");
        assert_eq!(
            custom.model.as_deref(),
            Some("p2/m2"),
            "explicit frontmatter model wins over state"
        );
        assert_eq!(
            custom.thought_level, None,
            "whole state entry skipped for profiles with explicit model"
        );
    }

    #[test]
    fn set_model_override_roundtrip() {
        let tmp = TempDir::new("state-rw");
        let data_dir = tmp.0.join("data"); // does not exist: set_model_override must create_dir_all first
        set_model_override(&data_dir, "explore", Some("p1/m1"), Some("high"))
            .expect("set override");
        let profiles = load_profiles(&tmp.0.join("proj"), &data_dir);
        let explore = find_profile(&profiles, "explore").expect("explore");
        assert_eq!(explore.model.as_deref(), Some("p1/m1"));
        assert_eq!(explore.thought_level.as_deref(), Some("high"));
        // both keys None = delete the entry for that name
        set_model_override(&data_dir, "explore", None, None).expect("delete override");
        let profiles = load_profiles(&tmp.0.join("proj"), &data_dir);
        let explore = find_profile(&profiles, "explore").expect("explore");
        assert_eq!(explore.model, None, "delete restores inherit");
        assert_eq!(explore.thought_level, None);
        // the file must still be valid JSON (a corrupt state file would make load silently drop overrides — guard this)
        let raw = std::fs::read_to_string(data_dir.join("agents-state.json")).unwrap();
        serde_json::from_str::<serde_json::Value>(&raw).expect("state file should be valid JSON");
    }

    // ---------- find_profile ----------

    fn three_profiles() -> Vec<AgentProfile> {
        let mut profiles = builtin_profiles();
        profiles.push(subagent(None, None)); // name = "tester"
        profiles
    }

    #[test]
    fn find_exact_match() {
        let profiles = three_profiles();
        assert_eq!(find_profile(&profiles, "explore").unwrap().name, "explore");
    }

    #[test]
    fn find_normalized_match() {
        let profiles = three_profiles();
        for query in [
            "General-Purpose",
            "general purpose",
            "GENERAL_PURPOSE",
            "generalpurpose",
        ] {
            let found = find_profile(&profiles, query)
                .unwrap_or_else(|e| panic!("{query} should match: {e}"));
            assert_eq!(
                found.name, "general-purpose",
                "normalized match for {query}"
            );
        }
    }

    #[test]
    fn find_ambiguous_err_lists_candidates() {
        // "my-agent" and "my_agent" normalize identically -> the normalized query hits both
        let mut a = subagent(None, None);
        a.name = "my-agent".into();
        let mut b = subagent(None, None);
        b.name = "my_agent".into();
        let err = find_profile(&[a, b], "myagent").unwrap_err();
        assert!(
            err.contains("my-agent") && err.contains("my_agent"),
            "should list candidates: {err}"
        );
    }

    #[test]
    fn find_missing_err_lists_all() {
        let profiles = three_profiles();
        let err = find_profile(&profiles, "nope").unwrap_err();
        for name in ["explore", "general-purpose", "tester"] {
            assert!(err.contains(name), "should list available {name}: {err}");
        }
    }

    // ---------- child_tool_set ----------

    fn all_tool_names() -> Vec<String> {
        crate::tool::all()
            .iter()
            .map(|t| t.name().to_string())
            .collect()
    }

    #[test]
    fn child_set_explore_read_only() {
        let explore = &builtin_profiles()[1];
        assert_eq!(explore.name, "explore");
        let tools = child_tool_set(
            explore,
            &all_tool_names(),
            crate::tool::MediaCapabilities::new(true, false),
        );
        for expected in [
            "Read",
            "Glob",
            "Grep",
            "ExecCommand",
            "FetchURL",
            "ReadMediaFile",
        ] {
            assert!(
                tools.contains(&expected.to_string()),
                "explore should have {expected}"
            );
        }
        for banned in [
            "Write",
            "Edit",
            "Agent",
            "AgentSwarm",
            "AskUserQuestion",
            "EnterPlanMode",
            "ExitPlanMode",
            "TodoList",
        ] {
            assert!(
                !tools.contains(&banned.to_string()),
                "explore should not have {banned}"
            );
        }
    }

    #[test]
    fn child_set_none_tools_all_minus_forbidden() {
        let general = &builtin_profiles()[0];
        assert_eq!(general.name, "general-purpose");
        let tools = child_tool_set(
            general,
            &all_tool_names(),
            crate::tool::MediaCapabilities::new(true, false),
        );
        assert!(
            tools.contains(&crate::tool::EXEC_COMMAND_TOOL.to_string()),
            "all tools should include the shell tool"
        );
        assert!(
            tools.contains(&"Write".to_string()),
            "all tools should include Write"
        );
        assert!(
            tools.contains(&"TodoList".to_string()),
            "all tools should include the child-local TodoList"
        );
        assert!(
            !tools.contains(&"Agent".to_string()),
            "Agent always removed"
        );
        assert!(
            !tools.contains(&"AgentSwarm".to_string()),
            "AgentSwarm always removed"
        );
        assert!(!tools.contains(&"AskUserQuestion".to_string()));
        assert!(!tools.contains(&"EnterPlanMode".to_string()));
        assert!(!tools.contains(&"ExitPlanMode".to_string()));
    }

    #[test]
    fn child_set_explicit_list_intersection() {
        // list ∩ all; nonexistent names ignored; FORBIDDEN removed even when explicitly listed
        let mut profile = subagent(None, None);
        profile.tools = Some(vec![
            "Read".into(),
            "NotExist".into(),
            "Agent".into(),
            "AgentSwarm".into(),
            "ExecCommand".into(),
        ]);
        let tools = child_tool_set(
            &profile,
            &all_tool_names(),
            crate::tool::MediaCapabilities::new(true, false),
        );
        assert!(tools.contains(&"Read".to_string()));
        assert!(tools.contains(&crate::tool::EXEC_COMMAND_TOOL.to_string()));
        assert!(
            !tools.contains(&"NotExist".to_string()),
            "nonexistent tool names ignored"
        );
        assert!(
            !tools.contains(&"Agent".to_string()),
            "explicitly listed Agent still removed"
        );
        assert!(
            !tools.contains(&"AgentSwarm".to_string()),
            "explicitly listed AgentSwarm still removed"
        );
        assert!(
            !tools.contains(&"Write".to_string()),
            "not in the list, not given"
        );
    }

    #[test]
    fn child_set_star_means_all() {
        let mut profile = subagent(None, None);
        profile.tools = Some(vec!["*".into()]);
        let tools = child_tool_set(
            &profile,
            &all_tool_names(),
            crate::tool::MediaCapabilities::new(true, false),
        );
        assert!(tools.contains(&"Write".to_string()), "* means all");
        assert!(!tools.contains(&"Agent".to_string()));
    }

    #[test]
    fn child_set_input_image_gates_media_tool() {
        let general = &builtin_profiles()[0];
        let with = child_tool_set(
            general,
            &all_tool_names(),
            crate::tool::MediaCapabilities::new(true, false),
        );
        assert!(
            with.contains(&"ReadMediaFile".to_string()),
            "kept when images supported"
        );
        let without = child_tool_set(
            general,
            &all_tool_names(),
            crate::tool::MediaCapabilities::new(false, false),
        );
        assert!(
            !without.contains(&"ReadMediaFile".to_string()),
            "removed when neither media capability is supported"
        );
        let pdf_only = child_tool_set(
            general,
            &all_tool_names(),
            crate::tool::MediaCapabilities::new(false, true),
        );
        assert!(
            pdf_only.contains(&"ReadMediaFile".to_string()),
            "PDF input alone keeps the shared media tool"
        );
    }

    // ---------- child_inherits_all_mcp ----------

    #[test]
    fn mcp_inherit_all_for_full_profile() {
        // general-purpose (tools=None -> all builtins after narrowing): includes Write/Edit -> inherits all MCP
        let general = &builtin_profiles()[0];
        let keep = child_tool_set(
            general,
            &all_tool_names(),
            crate::tool::MediaCapabilities::new(true, false),
        );
        assert!(
            child_inherits_all_mcp(&keep),
            "full-tool profile inherits all MCP tools"
        );
    }

    #[test]
    fn mcp_inherit_readonly_for_readonly_profile() {
        // explore narrowed down lacks Write/Edit (Bash does not affect the check) -> inherits only readOnlyHint
        let explore = &builtin_profiles()[1];
        let keep = child_tool_set(
            explore,
            &all_tool_names(),
            crate::tool::MediaCapabilities::new(true, false),
        );
        assert!(
            keep.contains(&crate::tool::EXEC_COMMAND_TOOL.to_string()),
            "explore includes the shell tool"
        );
        assert!(
            !child_inherits_all_mcp(&keep),
            "read-only profile inherits only readOnlyHint MCP tools"
        );
        // custom profile whose explicit list includes Write -> inherits all
        let mut writer = subagent(None, None);
        writer.tools = Some(vec!["Read".into(), "Write".into()]);
        let keep = child_tool_set(
            &writer,
            &all_tool_names(),
            crate::tool::MediaCapabilities::new(true, false),
        );
        assert!(child_inherits_all_mcp(&keep));
    }

    // ---------- misc ----------

    #[test]
    fn builtin_prompts_have_delivery_suffix() {
        for p in builtin_profiles() {
            assert!(
                p.system_prompt.contains("You are a subagent"),
                "{} missing the fixed delivery suffix",
                p.name
            );
        }
    }

    #[test]
    fn description_list_format() {
        let list = agent_description_list(&builtin_profiles());
        assert!(
            list.contains("- general-purpose: "),
            "each line starts with - name:"
        );
        assert!(list.contains("(tools: all)"), "tools=None shows all");
        assert!(list.contains("- explore: "));
        assert!(
            list.contains("(tools: Read, Glob, Grep, ExecCommand, FetchURL, ReadMediaFile)"),
            "explicit list named tool by tool"
        );
    }

    /// TodoList is a normal child tool with a per-child handle; only the root
    /// session persists it and emits root TodoListChanged events.
    #[test]
    fn todo_list_is_a_child_local_tool() {
        let mut profile = subagent(None, None);
        profile.tools = Some(vec!["Read".into(), "TodoList".into()]);
        let tools = child_tool_set(
            &profile,
            &all_tool_names(),
            crate::tool::MediaCapabilities::new(true, false),
        );
        assert!(tools.contains(&"Read".to_string()));
        assert!(
            tools.contains(&"TodoList".to_string()),
            "a profile that asks for TodoList receives the child-local tool"
        );
        let all = child_tool_set(
            &builtin_profiles()[0],
            &all_tool_names(),
            crate::tool::MediaCapabilities::new(true, false),
        );
        assert!(
            all.contains(&"TodoList".to_string()),
            "an all-tools child profile receives TodoList"
        );
    }

    /// One session-original store: a foreground child and the parent share
    /// the first-seen baseline, the dirty queue and the turn scope, so a
    /// child's writes enter the parent's TurnChanges once. A detached child
    /// shares baselines but keeps a separate turn scope.
    #[test]
    fn change_tracker_shares_session_baselines_and_scopes_turn_changes() {
        let tmp = TempDir::new("tracker");
        let existing = tmp.0.join("existing.txt");
        let created = tmp.0.join("created.txt");
        let detached_path = tmp.0.join("detached.txt");
        std::fs::write(&existing, "before\n").expect("existing file");

        let mut parent = crate::tool::ChangeTracker::default();
        parent.snapshot(&existing).expect("parent snapshot");
        assert_eq!(parent.take_dirty(), vec![existing.clone()]);
        assert_eq!(
            parent.original(&existing),
            Some(Some("before\n".to_string()))
        );

        let mut child = parent.foreground_child();
        child
            .snapshot(&existing)
            .expect("the shared baseline is already tracked");
        assert!(
            parent.take_dirty().is_empty(),
            "a shared baseline is not persisted twice"
        );
        assert_eq!(
            child.original(&existing),
            Some(Some("before\n".to_string())),
            "the child sees the session's first-seen original"
        );
        child
            .snapshot(&created)
            .expect("snapshot before the child creates the file");
        assert_eq!(
            parent.take_dirty(),
            vec![created.clone()],
            "a child-created path is visible in the shared dirty queue"
        );
        assert_eq!(
            parent.original(&created),
            Some(None),
            "the parent immediately sees the child-created baseline"
        );

        std::fs::write(&existing, "parent\n").expect("parent write");
        std::fs::write(&created, "child\n").expect("child write");
        let changes = parent.take_turn_changes(&tmp.0);
        assert_eq!(changes.len(), 2, "one entry per path");
        assert_eq!(
            changes
                .iter()
                .find(|change| change.path == "existing.txt")
                .map(|change| change.unified_diff.as_str()),
            Some("--- a/existing.txt\n+++ b/existing.txt\n@@ -1 +1 @@\n-before\n+parent\n"),
            "foreground child snapshots share the parent's turn scope"
        );

        let mut detached = parent.detached_child();
        detached
            .snapshot(&detached_path)
            .expect("detached snapshot");
        std::fs::write(&detached_path, "background\n").expect("detached write");
        assert!(
            parent.take_turn_changes(&tmp.0).is_empty(),
            "a detached child must not enter the parent's current-turn summary"
        );
        assert_eq!(
            parent.original(&detached_path),
            Some(None),
            "detached children still share session baselines"
        );
    }

    /// Concurrent drivers cannot move the first-seen baseline: whichever
    /// thread snapshots first records the pre-write bytes, and the shared
    /// dirty/turn scopes still contain one entry.
    #[test]
    fn change_tracker_first_seen_baseline_survives_concurrent_drivers() {
        let tmp = TempDir::new("tracker-race");
        let path = tmp.0.join("race.txt");
        std::fs::write(&path, "v0\n").expect("seed");
        let mut parent = crate::tool::ChangeTracker::default();
        let mut first = parent.foreground_child();
        let mut second = parent.foreground_child();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        std::thread::scope(|scope| {
            let first_barrier = barrier.clone();
            let first_path = path.clone();
            scope.spawn(move || {
                first_barrier.wait();
                first.snapshot(&first_path).expect("first snapshot");
                std::fs::write(&first_path, "first\n").expect("first write");
            });
            let second_barrier = barrier.clone();
            let second_path = path.clone();
            scope.spawn(move || {
                second_barrier.wait();
                second.snapshot(&second_path).expect("second snapshot");
                std::fs::write(&second_path, "second\n").expect("second write");
            });
        });
        assert_eq!(
            parent.original(&path),
            Some(Some("v0\n".to_string())),
            "the first-seen original wins across concurrent drivers"
        );
        assert_eq!(
            parent.take_dirty(),
            vec![path.clone()],
            "the shared dirty queue has one entry, not one per driver"
        );
        let changes = parent.take_turn_changes(&tmp.0);
        assert_eq!(changes.len(), 1, "one net turn entry");
        assert!(
            changes[0].unified_diff.contains("-v0"),
            "the turn baseline is the pre-write state: {}",
            changes[0].unified_diff
        );
        assert!(
            changes[0].unified_diff.contains("+first")
                || changes[0].unified_diff.contains("+second"),
            "the net diff points at the final writer: {}",
            changes[0].unified_diff
        );
    }

    #[test]
    fn subagent_prompt_composition() {
        let tmp = TempDir::new("prompt");
        let cwd = tmp.0.join("proj");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(cwd.join("AGENTS.md"), "project rules").unwrap();
        let mut profile = subagent(None, None);
        profile.system_prompt = "profile body text.".into();
        let agents = crate::prompt::agents_md(&tmp.0, &cwd);
        let prompt = crate::prompt::subagent_system_prompt(&profile, &cwd, None, &agents, "");
        assert!(prompt.contains("<env>"), "should contain env block");
        assert!(
            prompt.contains("Workspace AGENTS.md"),
            "inject_agents_md=true should inject"
        );
        assert!(prompt.contains("project rules"));
        assert!(
            prompt.ends_with("</env>"),
            "env block ends the prompt (volatile last)"
        );
        assert!(
            prompt.contains("profile body text."),
            "profile body kept before env"
        );
        // with injection off, the AGENTS.md section is gone
        profile.inject_agents_md = false;
        let prompt = crate::prompt::subagent_system_prompt(&profile, &cwd, None, &agents, "");
        assert!(!prompt.contains("AGENTS.md"));
        assert!(prompt.ends_with("</env>"));
    }

    /// The profile listing in tool schemas is frozen per session: Agent/AgentSwarm descriptions
    /// embed the profile listing, and tools sit at the very front of the cache prefix — under
    /// the same snapshot, on-disk profile changes never change the schema bytes (only a new
    /// session's new snapshot reflects them)
    #[test]
    fn all_root_schema_freezes_profiles() {
        let tmp = TempDir::new("freeze");
        let agents_dir = tmp.0.join("ws").join(".pigcode").join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        std::fs::write(
            agents_dir.join("extra.md"),
            "---\nname: extra\ndescription: test profile\n---\nprofile body text.",
        )
        .unwrap();
        let ws = tmp.0.join("ws");
        let snapshot = load_profiles(&ws, &tmp.0);
        assert!(
            snapshot.iter().any(|p| p.name == "extra"),
            "project-level profile should be in the snapshot"
        );
        let plugins = crate::plugins::PluginRegistry::shared(tmp.0.clone());
        let schemas = |profiles: &[AgentProfile]| {
            serde_json::to_string(&crate::tool::schemas_root(&ws, profiles, &plugins)).unwrap()
        };
        let frozen = schemas(&snapshot);
        // after freezing, add/remove profiles on disk: schema bytes from the same snapshot stay unchanged
        std::fs::remove_file(agents_dir.join("extra.md")).unwrap();
        assert_eq!(
            frozen,
            schemas(&snapshot),
            "same snapshot keeps tools prefix bytes stable"
        );
        // only a new session's new snapshot reflects the change
        let fresh = load_profiles(&ws, &tmp.0);
        assert!(!fresh.iter().any(|p| p.name == "extra"));
        assert_ne!(frozen, schemas(&fresh));
    }
}
