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
    /// Root conversation that owns storage and events.
    pub(crate) session_id: &'a str,
    /// Immediate caller, which differs from the root for nested delegation.
    pub(crate) parent_agent_id: &'a str,
    pub(crate) parent_delegation_depth: u32,
    pub(crate) max_delegation_depth: u32,
    /// Permission state captured from the parent for this activation. Known
    /// approvals remain shared separately; this is the child's fixed scope.
    pub(crate) parent_permissions: pig_protocol::PermissionSettings,
    /// Live parent history used only by an explicit fork-context creation.
    pub(crate) parent_history: &'a [ChatMsg],
    /// Current profile listing used by delegation tools when this child may
    /// itself become a parent.
    pub(crate) profiles_summary: &'a str,
    /// Session state: writer claims and running-conflict detection for resume
    /// entries.
    pub(crate) state: &'a SessionToolState,
    /// Session MCP handle (None = not connected): MCP tools enter the child
    /// tool set per the inheritance rule.
    pub(crate) mcp: Option<&'a std::sync::Arc<crate::mcp::McpManager>>,
    pub(crate) plugin_tools: crate::plugins::PluginToolset,
}

/// Fully resolved, resource-independent policy for one child. Every creation
/// and resume path builds this before `Session` assembly; the values captured
/// here do not change underneath an active run.
pub(crate) struct ResolvedChildPolicy {
    pub(crate) profile: AgentProfile,
    pub(crate) model: ResolvedModel,
    pub(crate) max_turns: usize,
    pub(crate) delegation_depth: u32,
    pub(crate) context_mode: crate::rollout::ChildContextMode,
    pub(crate) inherited_record_count: Option<usize>,
    pub(crate) permissions: pig_protocol::PermissionSettings,
    /// Inheritance-rule snapshot (same policy for every child kind).
    pub(crate) mcp_inherits_all: bool,
    /// Workspace-trust decision for project skills (the same value that
    /// admitted project profiles and the skill listing in the child prompt).
    pub(crate) trusted: bool,
}

/// One fully prepared child: resolved policy plus the history, tools and
/// exclusive context writer needed to drive it.
pub(crate) struct ChildPrep {
    pub(crate) agent_id: String,
    pub(crate) run_id: String,
    pub(crate) parent_agent_id: String,
    pub(crate) cwd: PathBuf,
    pub(crate) policy: ResolvedChildPolicy,
    pub(crate) tools: Vec<Box<dyn crate::tool::Tool>>,
    pub(crate) schemas: Vec<serde_json::Value>,
    pub(crate) history: Vec<ChatMsg>,
    pub(crate) jsonl: PathBuf,
    pub(crate) profiles_summary: String,
    pub(crate) description: String,
    /// Session MCP handle (None = not connected): the concurrent driving
    /// closure fetches the inherited tools at GateCtx assembly time.
    pub(crate) mcp: Option<std::sync::Arc<crate::mcp::McpManager>>,
    pub(crate) plugin_tools: crate::plugins::PluginToolset,
    /// Exclusive JSONL writer claim, taken before the first context write and
    /// released when the driver ends (including cancellation/panic).
    pub(crate) writer: AgentWriterGuard,
}

impl ChildPrep {
    pub(crate) fn run_metadata(&self) -> crate::task::AgentRunMetadata {
        crate::task::AgentRunMetadata {
            agent_id: self.agent_id.clone(),
            run_id: self.run_id.clone(),
            parent_agent_id: self.parent_agent_id.clone(),
            delegation_depth: self.policy.delegation_depth,
            profile: self.policy.profile.name.clone(),
            usage: (0, 0, 0),
        }
    }
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

impl ResolvedChildPolicy {
    fn new(
        profile: AgentProfile,
        model: ResolvedModel,
        ctx: &ChildPrepCtx<'_>,
        delegation_depth: u32,
        context_mode: crate::rollout::ChildContextMode,
        inherited_record_count: Option<usize>,
        tools: &ChildTools,
    ) -> Self {
        Self {
            max_turns: profile.max_turns.unwrap_or(DEFAULT_MAX_TURNS),
            profile,
            model,
            delegation_depth,
            context_mode,
            inherited_record_count,
            permissions: ctx.parent_permissions,
            mcp_inherits_all: tools.mcp_inherits_all,
            trusted: tools.trusted,
        }
    }
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

fn profile_allows(profile: &AgentProfile, name: &str) -> bool {
    match &profile.tools {
        None => true,
        Some(tools) => tools.iter().any(|tool| tool == "*" || tool == name),
    }
}

fn validate_child_depth(ctx: &ChildPrepCtx<'_>, depth: u32) -> Result<(), String> {
    if depth > ctx.max_delegation_depth {
        return Err(format!(
            "Delegation depth {depth} exceeds the configured max_delegation_depth={}. Resume or spawn is refused; increase [agents].max_delegation_depth to allow it.",
            ctx.max_delegation_depth
        ));
    }
    Ok(())
}

/// Build the child prompt and narrowed tool set. Every child kind uses this
/// exact path: profile tools ∩ all - forbidden, MCP/plugin inheritance by the
/// write-capability rule, the trust-gated Skill tool, collision validation and
/// schemas derived from the final tool list. Delegation tools are added only
/// when both the profile and the remaining depth budget allow them.
fn child_tools(
    ctx: &ChildPrepCtx<'_>,
    profile: &AgentProfile,
    child_config: &ResolvedModel,
    delegation_depth: u32,
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
        &ctx.plugin_tools.registry(),
        trusted,
    )));
    if delegation_depth < ctx.max_delegation_depth {
        if profile_allows(profile, "Agent") {
            tools.push(Box::new(crate::tool::AgentTool::from_summary(
                ctx.profiles_summary,
            )));
        }
        if profile_allows(profile, "AgentSwarm") {
            tools.push(Box::new(crate::tool::AgentSwarmTool::from_summary(
                ctx.profiles_summary,
            )));
        }
        if profile_allows(profile, "SendMessage") {
            tools.push(Box::new(crate::tool::SendMessageTool));
        }
    }
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

fn fork_history(ctx: &ChildPrepCtx<'_>, profile: &AgentProfile, prompt: &str) -> Vec<ChatMsg> {
    let mut history = vec![ChatMsg::system(crate::prompt::subagent_system_prompt(
        profile,
        ctx.cwd,
        ctx.git_snapshot,
        ctx.agents_prompt,
        ctx.skills_prompt,
    ))];
    history.extend(
        ctx.parent_history
            .iter()
            .filter(|message| message.role != "system")
            .cloned(),
    );
    crate::agent::reconcile_tool_results(&mut history);
    history.push(ChatMsg::user(
        "The preceding messages are an inherited snapshot of the parent conversation, not actions performed by this child. Use them as context only; the new task begins after this boundary."
            .to_string(),
    ));
    history.push(ChatMsg::user(prompt.to_string()));
    history
}

fn child_meta(
    agent_id: &str,
    parent_session_id: &str,
    cwd: &Path,
    description: &str,
    policy: &ResolvedChildPolicy,
) -> crate::rollout::RolloutRecord {
    crate::rollout::RolloutRecord::subagent_meta_with_context(
        agent_id.to_string(),
        parent_session_id.to_string(),
        cwd.to_path_buf(),
        description.to_string(),
        policy.profile.name.clone(),
        policy.model.provider_name.clone(),
        policy.model.model.clone(),
        policy.delegation_depth,
        policy.context_mode,
        policy.inherited_record_count,
        crate::rollout::now_secs(),
    )
}

fn agents_directory(ctx: &ChildPrepCtx<'_>) -> Result<PathBuf, String> {
    prepared_agent_dir(&ctx.data_dir.join("sessions"), ctx.session_id)
}

fn persist_history(prep: &mut ChildPrep) -> Result<(), String> {
    let mut calls = std::collections::HashMap::new();
    for message in &mut prep.history {
        if message.role == "assistant" {
            for call in message.tool_calls.clone().unwrap_or_default() {
                calls.insert(
                    call.id.clone(),
                    pig_provider::ToolCall {
                        id: call.id,
                        name: call.function.name,
                        arguments: call.function.arguments,
                    },
                );
            }
        }
        let result = if message.role == "tool" {
            let call_id = message.tool_call_id.clone().unwrap_or_default();
            let call = calls.get(&call_id).cloned().ok_or_else(|| {
                format!("Cannot fork a tool result without its assistant call: {call_id}")
            })?;
            append_agent_tool_result(
                &prep.jsonl,
                &call,
                message,
                prep.policy.model.context_window,
                crate::agent::AgentMsgMeta::default(),
            )
        } else {
            append_agent_message(&prep.jsonl, message, prep.policy.model.context_window)
        };
        result?;
    }
    Ok(())
}

fn persist_fresh_context(prep: &mut ChildPrep, parent_session_id: &str) -> Result<(), String> {
    let meta = child_meta(
        &prep.agent_id,
        parent_session_id,
        &prep.cwd,
        &prep.description,
        &prep.policy,
    );
    if let Err(error) = append_agent_conversation_record(&prep.jsonl, &meta) {
        return Err(child_persist_failure_note(
            &prep.agent_id,
            &error,
            false,
            ChildPersistPhase::NotStarted,
        ));
    }
    if let Err(error) = persist_history(prep) {
        // The initial task context is incomplete, so this run is not
        // resumable; the caller retries the Agent call.
        return Err(child_persist_failure_note(
            &prep.agent_id,
            &error,
            false,
            ChildPersistPhase::NotStarted,
        ));
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
    prepare_child(
        ctx,
        profile,
        child_config,
        prompt,
        description,
        crate::rollout::ChildContextMode::Fresh,
        agent_seq,
    )
}

/// Prepare a child seeded with the parent's current effective-history snapshot.
/// The child receives its own JSONL and media directory; no inherited write can
/// reach the parent's rollout.
pub(crate) fn prepare_fork_child(
    ctx: &ChildPrepCtx<'_>,
    profile: AgentProfile,
    child_config: ResolvedModel,
    prompt: &str,
    description: &str,
    agent_seq: &mut u64,
) -> Result<ChildPrep, ChildPrepError> {
    prepare_child(
        ctx,
        profile,
        child_config,
        prompt,
        description,
        crate::rollout::ChildContextMode::Fork,
        agent_seq,
    )
}

fn prepare_child(
    ctx: &ChildPrepCtx<'_>,
    profile: AgentProfile,
    child_config: ResolvedModel,
    prompt: &str,
    description: &str,
    context_mode: crate::rollout::ChildContextMode,
    agent_seq: &mut u64,
) -> Result<ChildPrep, ChildPrepError> {
    let delegation_depth = ctx
        .parent_delegation_depth
        .checked_add(1)
        .ok_or_else(|| ChildPrepError::CallLevel("Delegation depth overflow".to_string()))
        .and_then(|depth| {
            validate_child_depth(ctx, depth)
                .map(|()| depth)
                .map_err(ChildPrepError::CallLevel)
        })?;
    let history = match context_mode {
        crate::rollout::ChildContextMode::Fresh => fresh_history(ctx, &profile, prompt),
        crate::rollout::ChildContextMode::Fork => fork_history(ctx, &profile, prompt),
    };
    let inherited_record_count = match context_mode {
        crate::rollout::ChildContextMode::Fresh => None,
        crate::rollout::ChildContextMode::Fork => Some(history.len().saturating_sub(3)),
    };
    let tools = child_tools(ctx, &profile, &child_config, delegation_depth)
        .map_err(ChildPrepError::CallLevel)?;
    let agents_dir = agents_directory(ctx).map_err(ChildPrepError::CallLevel)?;
    let (agent_id, writer) =
        allocate_agent_id(ctx.state, &agents_dir, agent_seq).map_err(ChildPrepError::CallLevel)?;
    let jsonl = agents_dir.join(format!("{agent_id}.jsonl"));
    let mut prep = ChildPrep {
        agent_id,
        run_id: crate::session::new_agent_run_id(),
        parent_agent_id: ctx.parent_agent_id.to_string(),
        cwd: ctx.cwd.to_path_buf(),
        policy: ResolvedChildPolicy::new(
            profile,
            child_config,
            ctx,
            delegation_depth,
            context_mode,
            inherited_record_count,
            &tools,
        ),
        tools: tools.tools,
        schemas: tools.schemas,
        history,
        jsonl,
        profiles_summary: ctx.profiles_summary.to_string(),
        description: description.to_string(),
        mcp: ctx.mcp.cloned(),
        plugin_tools: ctx.plugin_tools.clone(),
        writer,
    };
    persist_fresh_context(&mut prep, ctx.session_id).map_err(ChildPrepError::ItemLevel)?;
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
    let parent_depth = ctx.parent_delegation_depth.saturating_add(1);
    let delegation_depth = meta.delegation_depth.max(parent_depth);
    validate_child_depth(ctx, delegation_depth).map_err(ChildPrepError::ItemLevel)?;
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
    let tools = child_tools(ctx, &profile, &child_config, delegation_depth)
        .map_err(ChildPrepError::ItemLevel)?;
    let jsonl = agents_dir.join(format!("{agent_id}.jsonl"));
    let mut prep = ChildPrep {
        agent_id: agent_id.to_string(),
        run_id: crate::session::new_agent_run_id(),
        parent_agent_id: ctx.parent_agent_id.to_string(),
        cwd: ctx.cwd.to_path_buf(),
        policy: ResolvedChildPolicy::new(
            profile,
            child_config,
            ctx,
            delegation_depth,
            meta.context_mode,
            meta.inherited_record_count,
            &tools,
        ),
        tools: tools.tools,
        schemas: tools.schemas,
        history,
        jsonl,
        profiles_summary: ctx.profiles_summary.to_string(),
        description: description.to_string(),
        mcp: ctx.mcp.cloned(),
        plugin_tools: ctx.plugin_tools.clone(),
        writer,
    };
    if let Err(error) = append_agent_message(
        &prep.jsonl,
        prep.history.last_mut().expect("resume prompt pushed"),
        prep.policy.model.context_window,
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
    let profiles =
        load_profiles_with_plugins(ctx.cwd, ctx.data_dir, &trust, &ctx.plugin_tools.registry());
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
