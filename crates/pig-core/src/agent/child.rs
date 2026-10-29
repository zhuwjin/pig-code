//! One preparation pipeline for every child kind.
//!
//! A foreground Agent call, a background Agent, and each AgentSwarm item all
//! resolve the same trust decision, profile, model, narrowed tool set, MCP
//! inheritance, media projection and JSONL context here. The managed
//! lifecycle (writer claim, global slot, registry settlement, panic cleanup)
//! stays in `session::subagent`; this module never starts a driver.

use std::path::{Path, PathBuf};

use pig_provider::{ChatMsg, ResolvedModel};

use super::*;
use crate::config::{RuntimeConfig, WorkspaceTrust};
use crate::task::{AgentWriterGuard, SessionToolState};

/// Immutable session-level inputs for child preparation.
pub(crate) struct ChildPrepCtx<'a> {
    pub(crate) cwd: &'a Path,
    pub(crate) data_dir: &'a Path,
    pub(crate) git_snapshot: Option<&'a str>,
    /// Skills/AGENTS.md sections frozen for the session (injected into the
    /// child system prompt; the same copy as the main agent).
    pub(crate) skills_prompt: &'a str,
    pub(crate) agents_prompt: &'a str,
    /// One resolved runtime view (trust + provider/model capabilities) for
    /// every child in this batch; `None` = config not loaded (inherit only).
    pub(crate) runtime: Option<&'a RuntimeConfig>,
    pub(crate) parent_config: &'a ResolvedModel,
    pub(crate) session_id: &'a str,
    /// Session state: writer claims and running-conflict detection for resume
    /// entries.
    pub(crate) state: &'a SessionToolState,
    /// Session MCP handle (None = not connected): MCP tools enter the child
    /// tool set per the inheritance rule.
    pub(crate) mcp: Option<&'a std::sync::Arc<crate::mcp::McpManager>>,
    pub(crate) plugin_tools: crate::plugins::PluginToolset,
}

/// One fully prepared child: every input the drive needs, including the
/// exclusive context writer claim.
pub(crate) struct ChildPrep {
    pub(crate) agent_id: String,
    pub(crate) cwd: PathBuf,
    pub(crate) data_dir: PathBuf,
    pub(crate) profile: AgentProfile,
    pub(crate) child_config: ResolvedModel,
    pub(crate) tools: Vec<Box<dyn crate::tool::Tool>>,
    pub(crate) schemas: Vec<serde_json::Value>,
    pub(crate) history: Vec<ChatMsg>,
    pub(crate) jsonl: PathBuf,
    pub(crate) max_turns: usize,
    pub(crate) description: String,
    /// Session MCP handle (None = not connected): the concurrent driving
    /// closure fetches the inherited tools at GateCtx assembly time.
    pub(crate) mcp: Option<std::sync::Arc<crate::mcp::McpManager>>,
    pub(crate) plugin_tools: crate::plugins::PluginToolset,
    /// Inheritance-rule snapshot (same policy for every child kind).
    pub(crate) mcp_inherits_all: bool,
    /// Workspace-trust decision for project skills (the same value that
    /// admitted project profiles and the skill listing in the child prompt).
    pub(crate) trusted: bool,
    /// Exclusive JSONL writer claim, taken before the first context write and
    /// released when the driver ends (including cancellation/panic).
    pub(crate) writer: AgentWriterGuard,
}

/// Batch preparation result for one swarm item: a failure does not affect
/// other items (recorded as a preparation-phase failure at aggregation).
pub(crate) enum SwarmPrep {
    Ready(Box<ChildPrep>),
    Failed { description: String, error: String },
}

/// A preparation failure that must either fail the whole Agent/AgentSwarm
/// call (shared tool/profile/path errors) or only the affected item (its own
/// context write). This mirrors the single-Agent fail callback and the old
/// per-item settlement without string inspection.
#[derive(Debug)]
pub(crate) enum ChildPrepError {
    CallLevel(String),
    ItemLevel(String),
}

impl ChildPrepError {
    pub(crate) fn message(self) -> String {
        match self {
            Self::CallLevel(message) | Self::ItemLevel(message) => message,
        }
    }
}

struct ChildTools {
    tools: Vec<Box<dyn crate::tool::Tool>>,
    schemas: Vec<serde_json::Value>,
    mcp_inherits_all: bool,
    trusted: bool,
}

/// Resolve the child model through the one resolved runtime view: `None`
/// inherits the parent endpoint; an explicit model/level is strict. Without a
/// runtime snapshot, inheriting still works but an explicit model cannot be
/// resolved.
pub(crate) fn resolve_child_config(
    ctx: &ChildPrepCtx<'_>,
    profile: &AgentProfile,
) -> Result<ResolvedModel, String> {
    match ctx.runtime {
        Some(runtime) => runtime
            .resolve_subagent_model(
                ctx.parent_config,
                profile.model.as_deref(),
                profile.thought_level.as_deref(),
                &profile.name,
            )
            .map_err(String::from),
        None if profile.model.is_some() => {
            Err("App config not loaded; cannot resolve the subagent's specified model".to_string())
        }
        None => Ok(ctx.parent_config.clone()),
    }
}

/// Build the child prompt and narrowed tool set. Every child kind uses this
/// exact path: profile tools ∩ all - forbidden, MCP/plugin inheritance by the
/// write-capability rule, the trust-gated Skill tool, collision validation and
/// schemas derived from the final tool list.
fn child_tools(
    ctx: &ChildPrepCtx<'_>,
    profile: &AgentProfile,
    child_config: &ResolvedModel,
) -> Result<ChildTools, String> {
    let all_tools: Vec<Box<dyn crate::tool::Tool>> = crate::tool::all();
    let all_names: Vec<String> = all_tools.iter().map(|t| t.name().to_string()).collect();
    let keep = child_tool_set(
        profile,
        &all_names,
        crate::tool::MediaCapabilities::new(child_config.input_image, child_config.input_pdf),
    );
    let mut tools: Vec<Box<dyn crate::tool::Tool>> = all_tools
        .into_iter()
        .filter(|t| keep.iter().any(|name| name == t.name()))
        .collect();
    let mcp_inherits_all = child_inherits_all_mcp(&keep);
    if let Some(mcp) = ctx.mcp {
        tools.extend(mcp.child_tools(mcp_inherits_all));
    }
    tools.extend(
        ctx.plugin_tools
            .tools_filtered(&crate::plugins::PluginToolFilter::default())
            .into_iter()
            .filter(|tool| mcp_inherits_all || tool.read_only()),
    );
    let trusted = ctx
        .runtime
        .is_some_and(|runtime| runtime.trust().admits(ctx.cwd));
    // Skill is supplied to every child (same cross-profile default as MCP):
    // bodies load on demand and the child prompt lists the same skills. The
    // trust decision must match the listing that made a skill visible.
    tools.push(Box::new(crate::tool::SkillTool::new_trusted(
        ctx.cwd,
        ctx.data_dir,
        trusted,
    )));
    validate_child_tool_names(&tools)?;
    let schemas = tools
        .iter()
        .map(|t| crate::tool::schema_with_strict(t.as_ref()))
        .collect();
    Ok(ChildTools {
        tools,
        schemas,
        mcp_inherits_all,
        trusted,
    })
}

fn fresh_history(ctx: &ChildPrepCtx<'_>, profile: &AgentProfile, prompt: &str) -> Vec<ChatMsg> {
    vec![
        ChatMsg::system(crate::prompt::subagent_system_prompt(
            profile,
            ctx.cwd,
            ctx.git_snapshot,
            ctx.agents_prompt,
            ctx.skills_prompt,
        )),
        ChatMsg::user(prompt.to_string()),
    ]
}

fn child_meta(
    agent_id: &str,
    profile: &AgentProfile,
    child_config: &ResolvedModel,
    description: &str,
) -> serde_json::Value {
    serde_json::json!({
        "type": "meta",
        "agent_id": agent_id,
        "profile": &profile.name,
        "description": description,
        "model": &child_config.model,
        "provider": &child_config.provider_name,
        "created_at": crate::rollout::now_secs(),
    })
}

fn agents_directory(ctx: &ChildPrepCtx<'_>) -> Result<PathBuf, String> {
    prepared_agent_dir(&ctx.data_dir.join("sessions"), ctx.session_id)
}

fn persist_fresh_context(prep: &mut ChildPrep) -> Result<(), String> {
    let meta = child_meta(
        &prep.agent_id,
        &prep.profile,
        &prep.child_config,
        &prep.description,
    );
    if let Err(error) = append_agent_record(&prep.jsonl, &meta) {
        return Err(child_persist_failure_note(
            &prep.agent_id,
            &error,
            false,
            ChildPersistPhase::NotStarted,
        ));
    }
    for message in &mut prep.history {
        if let Err(error) =
            append_agent_message(&prep.jsonl, message, prep.child_config.context_window)
        {
            // The initial task context is incomplete, so this run is not
            // resumable; the caller retries the Agent call.
            return Err(child_persist_failure_note(
                &prep.agent_id,
                &error,
                false,
                ChildPersistPhase::NotStarted,
            ));
        }
    }
    Ok(())
}

/// Prepare a fresh child context: allocate a unique id and writer, build the
/// prompt and tools, and persist the meta + initial messages before the child
/// can start. A persistence failure releases the writer by dropping the local
/// claim and returns the one model-facing failure note.
pub(crate) fn prepare_fresh_child(
    ctx: &ChildPrepCtx<'_>,
    profile: AgentProfile,
    child_config: ResolvedModel,
    prompt: &str,
    description: &str,
    agent_seq: &mut u64,
) -> Result<ChildPrep, ChildPrepError> {
    let history = fresh_history(ctx, &profile, prompt);
    let tools = child_tools(ctx, &profile, &child_config).map_err(ChildPrepError::CallLevel)?;
    let agents_dir = agents_directory(ctx).map_err(ChildPrepError::CallLevel)?;
    let max_turns = profile.max_turns.unwrap_or(DEFAULT_MAX_TURNS);
    let (agent_id, writer) =
        allocate_agent_id(ctx.state, &agents_dir, agent_seq).map_err(ChildPrepError::CallLevel)?;
    let jsonl = agents_dir.join(format!("{agent_id}.jsonl"));
    let mut prep = ChildPrep {
        agent_id,
        cwd: ctx.cwd.to_path_buf(),
        data_dir: ctx.data_dir.to_path_buf(),
        profile,
        child_config,
        tools: tools.tools,
        schemas: tools.schemas,
        history,
        jsonl,
        max_turns,
        description: description.to_string(),
        mcp: ctx.mcp.cloned(),
        plugin_tools: ctx.plugin_tools.clone(),
        mcp_inherits_all: tools.mcp_inherits_all,
        trusted: tools.trusted,
        writer,
    };
    persist_fresh_context(&mut prep).map_err(ChildPrepError::ItemLevel)?;
    Ok(prep)
}

/// Prepare a resume: claim the writer before reading, reload the context,
/// re-resolve profile/model from current state, project media for the current
/// model, append the new prompt, then build the same narrowed tool set. Every
/// failure releases the claim and returns the one model-facing failure note.
pub(crate) fn prepare_resume_child(
    ctx: &ChildPrepCtx<'_>,
    profiles: &[AgentProfile],
    agent_id: &str,
    prompt: &str,
    description: &str,
) -> Result<ChildPrep, ChildPrepError> {
    let agents_dir = agents_directory(ctx).map_err(ChildPrepError::CallLevel)?;
    let writer = claim_resume_writer(ctx.state, agent_id).map_err(ChildPrepError::ItemLevel)?;
    let sessions_dir = ctx.data_dir.join("sessions");
    let (meta, mut history) = read_agent_by_id(&sessions_dir, ctx.session_id, agent_id)
        .map_err(ChildPrepError::ItemLevel)?;
    // Re-find the profile from current state (deleted → error); the model is
    // likewise re-resolved from the profile's current state (meta.model is
    // ignored).
    let profile = find_profile(profiles, &meta.profile)
        .map_err(ChildPrepError::ItemLevel)?
        .clone();
    history.push(ChatMsg::user(prompt.to_string()));
    let child_config = resolve_child_config(ctx, &profile).map_err(ChildPrepError::ItemLevel)?;
    project_history_media(
        &mut history,
        crate::tool::MediaCapabilities::new(child_config.input_image, child_config.input_pdf),
    );
    crate::agent::fill_document_token_estimates(&mut history, child_config.context_window);
    let tools = child_tools(ctx, &profile, &child_config).map_err(ChildPrepError::ItemLevel)?;
    let max_turns = profile.max_turns.unwrap_or(DEFAULT_MAX_TURNS);
    let jsonl = agents_dir.join(format!("{agent_id}.jsonl"));
    let mut prep = ChildPrep {
        agent_id: agent_id.to_string(),
        cwd: ctx.cwd.to_path_buf(),
        data_dir: ctx.data_dir.to_path_buf(),
        profile,
        child_config,
        tools: tools.tools,
        schemas: tools.schemas,
        history,
        jsonl,
        max_turns,
        description: description.to_string(),
        mcp: ctx.mcp.cloned(),
        plugin_tools: ctx.plugin_tools.clone(),
        mcp_inherits_all: tools.mcp_inherits_all,
        trusted: tools.trusted,
        writer,
    };
    if let Err(error) = append_agent_message(
        &prep.jsonl,
        prep.history.last_mut().expect("resume prompt pushed"),
        prep.child_config.context_window,
    ) {
        // The existing context is intact up to this write; refuse the resume
        // instead of running with a live prompt the JSONL does not contain.
        return Err(ChildPrepError::ItemLevel(child_persist_failure_note(
            &prep.agent_id,
            &error,
            true,
            ChildPersistPhase::NotStarted,
        )));
    }
    Ok(prep)
}

/// Whether the child was refused before it started or was stopped mid-run
/// after a sticky context-write failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChildPersistPhase {
    NotStarted,
    Stopped,
}

/// Model-facing failure for a child whose context could not be persisted. The
/// child never starts (or its resume is refused) or is stopped before its live
/// history can diverge from the saved JSONL.
pub(crate) fn child_persist_failure_note(
    agent_id: &str,
    error: &str,
    resumable: bool,
    phase: ChildPersistPhase,
) -> String {
    let recovery = if resumable {
        format!(
            "The saved context is intact up to the failed write. Fix the disk problem, then \
             continue with Agent(resume=\"{agent_id}\", prompt=\"...\"); a torn final line is \
             repaired on resume."
        )
    } else {
        "The context file is incomplete, so this run cannot be resumed; retry the Agent call \
         once the disk is writable."
            .to_string()
    };
    let state = match phase {
        ChildPersistPhase::NotStarted => {
            "The subagent did not start with a live conversation the saved context does not contain."
        }
        ChildPersistPhase::Stopped => {
            "The subagent was stopped before its live conversation diverged from the saved context."
        }
    };
    format!("Subagent {agent_id} could not persist its context: {error}. {state} {recovery}")
}

/// Batch-prepare swarm contexts through the same single-child builder. Item
/// subtasks share one profile+model resolution — profile/model errors are
/// call-level argument errors that fail the whole call; resume entries are
/// re-resolved individually so one failure is recorded as that entry's Failed
/// without affecting the others.
pub(crate) fn prepare_swarm_children(
    ctx: &ChildPrepCtx<'_>,
    plan: &crate::tool::SwarmPlan,
    agent_seq: &mut u64,
) -> Result<Vec<SwarmPrep>, String> {
    let trust = match ctx.runtime {
        Some(runtime) => runtime.trust().clone(),
        None => WorkspaceTrust::untrusted(ctx.cwd),
    };
    let profiles = load_profiles_with_trust(ctx.cwd, ctx.data_dir, &trust);
    let item_profile = if plan.item_count() > 0 {
        Some(find_profile(&profiles, &plan.subagent_type)?.clone())
    } else {
        None
    };
    let item_config = match &item_profile {
        Some(profile) => Some(resolve_child_config(ctx, profile)?),
        None => None,
    };
    let mut preps = Vec::new();
    for task in &plan.tasks {
        match &task.resume {
            None => {
                let profile = item_profile
                    .clone()
                    .expect("item subtask must have a profile");
                let child_config = item_config
                    .clone()
                    .expect("item subtask must have a model config");
                match prepare_fresh_child(
                    ctx,
                    profile,
                    child_config,
                    &task.prompt,
                    &task.description,
                    agent_seq,
                ) {
                    Ok(prep) => preps.push(SwarmPrep::Ready(Box::new(prep))),
                    Err(ChildPrepError::CallLevel(error)) => return Err(error),
                    Err(ChildPrepError::ItemLevel(error)) => preps.push(SwarmPrep::Failed {
                        description: task.description.clone(),
                        error,
                    }),
                }
            }
            Some(resume_id) => match prepare_resume_child(
                ctx,
                &profiles,
                resume_id,
                &task.prompt,
                &task.description,
            ) {
                Ok(prep) => preps.push(SwarmPrep::Ready(Box::new(prep))),
                Err(ChildPrepError::CallLevel(error)) => return Err(error),
                Err(ChildPrepError::ItemLevel(error)) => preps.push(SwarmPrep::Failed {
                    description: task.description.clone(),
                    error,
                }),
            },
        }
    }
    Ok(preps)
}
