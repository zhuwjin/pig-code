//! Execution side of AgentSwarm batch-parallel subagents: subtask context preparation
//! (reusing the same profile/model/tool-narrowing/JSONL persistence pipeline as
//! run_subagent) and aggregated result formatting. Argument expansion/validation lives in
//! tool::parse_swarm_args (a pure function at the schema layer); concurrent driving lives in
//! the session layer (drive_subagent, private); the global concurrency slots live in task.rs.

use super::*;
use pig_provider::ChatMsg;

/// Total budget for the aggregated result (same as the 32K budget for a single subagent's result injected into the parent session)
pub const SWARM_RESULT_BUDGET: usize = 32_000;
/// Per-subagent preview cap inside the aggregated result (truncated beyond this; full text in each result.md)
pub const SWARM_CHILD_PREVIEW: usize = 3_000;

/// Final status of a single subagent in an AgentSwarm
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SwarmChildStatus {
    Completed,
    Failed,
    /// Parent turn cancelled / TaskStop
    Cancelled,
}

impl SwarmChildStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Single-subagent result for aggregation (constructed by the session layer after driving finishes; preparation-phase failures have no agent_id)
pub struct SwarmChildResult {
    pub description: String,
    pub agent_id: Option<String>,
    pub status: SwarmChildStatus,
    pub turns: usize,
    /// Path of the full result (when result.md was persisted and not cancelled)
    pub result_path: Option<PathBuf>,
    /// Result text (already truncated to 32K by the driver; aggregation adds the SWARM_CHILD_PREVIEW cap)
    pub result_text: String,
    /// Whether it waited in the global concurrency-slot queue (for the queue note in the aggregation header)
    pub queued: bool,
    /// Token usage (input, cache_read, output): foreground swarms accumulate into the parent turn's stats
    pub usage: (u64, u64, u64),
}

/// Aggregated result as a single tool result: header stats (completed/failed/cancelled +
/// concurrency-slot queueing), then one section per subtask (description, agent_id, status,
/// result-file pointer, preview). Total length is truncated to SWARM_RESULT_BUDGET —
/// overflowing sections degrade to pointer lines (each item still reaches its result.md).
pub fn format_swarm_result(children: &[SwarmChildResult]) -> String {
    let total = children.len();
    let completed = children
        .iter()
        .filter(|c| c.status == SwarmChildStatus::Completed)
        .count();
    let failed = children
        .iter()
        .filter(|c| c.status == SwarmChildStatus::Failed)
        .count();
    let cancelled = total - completed - failed;
    let queued = children.iter().filter(|c| c.queued).count();
    let queue_note = if queued > 0 {
        format!(
            "; global concurrency limit {}, {queued} waited in queue for a free slot",
            crate::task::MAX_CONCURRENT_SUBAGENTS
        )
    } else {
        String::new()
    };
    let mut out = format!(
        "Swarm finished: {total} subagents total ({completed} completed / {failed} failed / \
         {cancelled} cancelled){queue_note}.\n\
         Each item's full result is in the result.md file listed below — use Read for the \
         complete content; Agent(resume=\"agent_id\", prompt=\"...\") resumes a specific item.\n"
    );
    for (ix, child) in children.iter().enumerate() {
        let section = format_child_section(ix + 1, child);
        if out.chars().count() + section.chars().count() > SWARM_RESULT_BUDGET {
            // Budget exhausted: this section and all later ones degrade to pointer lines; the
            // pointer tail must stay complete (each item's status and result.md remain
            // reachable), and the head makes room for it
            let mut tail = String::from(
                "\n[Aggregated result too long; the subagents below keep only status and result-file pointers:]\n",
            );
            for rest in &children[ix..] {
                tail.push_str(&child_pointer_line(rest));
                tail.push('\n');
            }
            let head_budget = SWARM_RESULT_BUDGET.saturating_sub(tail.chars().count());
            if out.chars().count() > head_budget {
                out = out.chars().take(head_budget).collect();
            }
            out.push_str(&tail);
            return out;
        }
        out.push_str(&section);
    }
    out
}

/// One subtask section in the aggregated result: description heading + agent_id/status/turns + result-file pointer + preview
fn format_child_section(n: usize, child: &SwarmChildResult) -> String {
    let mut section = format!("\n## {n}. {}\n", child.description);
    match &child.agent_id {
        Some(agent_id) => section.push_str(&format!(
            "agent_id: {agent_id} · status: {} · turns: {}\n",
            child.status.label(),
            child.turns
        )),
        None => section.push_str(&format!(
            "status: {} (failed during preparation, not started)\n",
            child.status.label()
        )),
    }
    if let Some(path) = &child.result_path {
        section.push_str(&format!("Full result: {}\n", path.display()));
    }
    let count = child.result_text.chars().count();
    if count == 0 {
        let placeholder = match child.status {
            SwarmChildStatus::Cancelled => "(stopped, no result)",
            _ => "(no result text)",
        };
        section.push_str(placeholder);
    } else if count > SWARM_CHILD_PREVIEW {
        let preview: String = child
            .result_text
            .chars()
            .take(SWARM_CHILD_PREVIEW)
            .collect();
        let hint = match &child.result_path {
            Some(path) => format!("\n[Preview truncated; full result: {}]", path.display()),
            None => "\n[Preview truncated]".to_string(),
        };
        section.push_str(&preview);
        section.push_str(&hint);
    } else {
        section.push_str(&child.result_text);
    }
    section.push('\n');
    section
}

/// One-line pointer on budget overflow (description is already capped at 60 chars; line length is far below the budgeted share)
fn child_pointer_line(child: &SwarmChildResult) -> String {
    let base = match &child.agent_id {
        Some(agent_id) => format!(
            "- {} ({agent_id}, status: {})",
            child.description,
            child.status.label()
        ),
        None => format!(
            "- {} (status: {}, failed during preparation)",
            child.description,
            child.status.label()
        ),
    };
    match &child.result_path {
        Some(path) => format!("{base}: {}", path.display()),
        None => base,
    }
}

/// Full execution prep for one subagent (all inputs of SubagentDrive; the session layer assembles the driver)
pub struct SwarmChildPrep {
    pub agent_id: String,
    pub cwd: PathBuf,
    pub data_dir: PathBuf,
    pub profile: AgentProfile,
    pub child_config: ResolvedModel,
    pub tools: Vec<Box<dyn crate::tool::Tool>>,
    pub schemas: Vec<serde_json::Value>,
    pub history: Vec<ChatMsg>,
    pub jsonl: PathBuf,
    pub max_turns: usize,
    pub description: String,
    /// Session MCP handle (None = not connected): the concurrent driving closure fetches the inherited tools at the GateCtx assembly point
    pub mcp: Option<std::sync::Arc<crate::mcp::McpManager>>,
    /// Inheritance-rule snapshot (same policy as run_subagent)
    pub mcp_inherits_all: bool,
}

/// Preparation result for a single subtask: a failure does not affect other subtasks (recorded as a preparation-phase failure at aggregation)
pub enum SwarmPrep {
    Ready(Box<SwarmChildPrep>),
    Failed { description: String, error: String },
}

/// One entry in a background swarm's immediate receipt (a dispatched Ready subagent, or a preparation-phase failure)
pub struct SwarmReceiptChild {
    pub description: String,
    /// agent_id of the dispatched subagent (None = preparation-phase failure, not started; error carries the reason)
    pub agent_id: Option<String>,
    /// Background task task_id of the dispatched subagent (present iff agent_id is)
    pub task_id: Option<String>,
    /// Queue state estimated from free concurrency slots at receipt-assembly time (true = queued waiting for a free slot; instantaneous, informational only)
    pub queued: bool,
    /// Preparation-phase failure reason
    pub error: Option<String>,
}

/// Immediate receipt of a background swarm (a pure function for unit testing): header stats +
/// per-item agent_id/task_id/status (queued/running), with preparation-phase failures listed
/// too; tells the model completions arrive one by one via <task-notification> and must not be
/// polled (same policy as the single background Agent receipt).
pub fn format_swarm_receipt(children: &[SwarmReceiptChild]) -> String {
    let total = children.len();
    let queued = children.iter().filter(|c| c.queued).count();
    let failed = children.iter().filter(|c| c.agent_id.is_none()).count();
    let mut notes = String::new();
    if queued > 0 {
        notes.push_str(&format!(
            "; global concurrency limit {}, {queued} will queue for a free slot first",
            crate::task::MAX_CONCURRENT_SUBAGENTS
        ));
    }
    if failed > 0 {
        notes.push_str(&format!(
            "; {failed} failed during preparation (not started; see the list below)"
        ));
    }
    let mut out = format!(
        "Swarm launched in the background: {total} subagents total{notes}.\n\
         Each item's completion or failure arrives as its own <task-notification> — do not \
         poll; the full result is in the file the notification points to (read it with Read).\n\
         Use TaskList to list tasks, TaskOutput to check progress, TaskStop to stop one, and \
         Agent(resume=\"agent_id\", prompt=\"...\") to resume a specific item.\n"
    );
    for (ix, child) in children.iter().enumerate() {
        out.push_str(&format!("\n{}. {}\n", ix + 1, child.description));
        match (&child.agent_id, &child.task_id) {
            (Some(agent_id), Some(task_id)) => out.push_str(&format!(
                "agent_id: {agent_id} · task_id: {task_id} · status: {}\n",
                if child.queued { "queued" } else { "running" }
            )),
            _ => out.push_str(&format!(
                "status: failed (failed during preparation, not started): {}\n",
                child.error.as_deref().unwrap_or("unknown error")
            )),
        }
    }
    out
}

/// Input bundle for prepare_swarm_children (a snapshot of Session fields, all immutable borrows)
pub struct SwarmPrepCtx<'a> {
    pub cwd: &'a Path,
    pub data_dir: &'a Path,
    pub git_snapshot: Option<&'a str>,
    /// Skills/AGENTS.md sections frozen for the session (injected into the subagent system prompt; same copy as the main agent)
    pub skills_prompt: &'a str,
    pub agents_prompt: &'a str,
    pub app_config: Option<&'a AppConfig>,
    pub parent_config: &'a ResolvedModel,
    pub session_id: &'a str,
    /// Running-conflict detection for resume entries (a registry entry with the same agent_id and Running -> reject parallel continuation)
    pub tasks: &'a crate::task::TaskRegistry,
    /// Session MCP handle (None = not connected): MCP tools enter the subagent tool set per the inheritance rule
    pub mcp: Option<&'a std::sync::Arc<crate::mcp::McpManager>>,
}

/// Batch-prepare subagent contexts (same pipeline as run_subagent: profile/model/tool
/// narrowing/JSONL persistence). Item subtasks share one profile+model resolution —
/// profile/model errors are call-level argument errors that fail the whole call with Err
/// surfaced immediately (strict semantics of resolve_subagent_model); resume entries are each
/// re-resolved as-is, and a single failure (nonexistent/running/profile deleted) is recorded
/// as that entry's Failed without affecting the others.
pub fn prepare_swarm_children(
    ctx: &SwarmPrepCtx<'_>,
    plan: &crate::tool::SwarmPlan,
    agent_seq: &mut u64,
) -> Result<Vec<SwarmPrep>, String> {
    let profiles = load_profiles(ctx.cwd, ctx.data_dir);
    let agents_dir = agents_dir(&ctx.data_dir.join("sessions"), ctx.session_id);
    // items share the profile/model: only resolve when there are items (pure resume never touches subagent_type)
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
                *agent_seq += 1;
                let agent_id = format!("a{}-{}", crate::rollout::now_secs(), *agent_seq);
                let history = vec![
                    ChatMsg::system(crate::prompt::subagent_system_prompt(
                        &profile,
                        ctx.cwd,
                        ctx.git_snapshot,
                        ctx.agents_prompt,
                        ctx.skills_prompt,
                    )),
                    ChatMsg::user(task.prompt.clone()),
                ];
                let jsonl = agents_dir.join(format!("{agent_id}.jsonl"));
                persist_line(
                    &jsonl,
                    &serde_json::json!({
                        "type": "meta",
                        "agent_id": agent_id,
                        "profile": profile.name,
                        "description": task.description,
                        "model": child_config.model,
                        "provider": child_config.provider_name,
                        "created_at": crate::rollout::now_secs(),
                    }),
                );
                for msg in &history {
                    persist_msg(&jsonl, msg);
                }
                preps.push(SwarmPrep::Ready(Box::new(assemble_prep(
                    agent_id,
                    profile,
                    child_config,
                    history,
                    jsonl,
                    task.description.clone(),
                    ctx,
                ))));
            }
            Some(resume_id) => match prepare_resume(ctx, &profiles, &agents_dir, resume_id, task) {
                Ok(prep) => preps.push(SwarmPrep::Ready(Box::new(prep))),
                Err(error) => preps.push(SwarmPrep::Failed {
                    description: task.description.clone(),
                    error,
                }),
            },
        }
    }
    Ok(preps)
}

/// Resume-subtask prep: read the existing context + append the new prompt (profile/model
/// re-resolved as-is, same policy as run_subagent's resume branch); on model-resolution
/// failure nothing is appended to the persisted file.
fn prepare_resume(
    ctx: &SwarmPrepCtx<'_>,
    profiles: &[AgentProfile],
    agents_dir: &Path,
    resume_id: &str,
    task: &crate::tool::SwarmTask,
) -> Result<SwarmChildPrep, String> {
    let jsonl = agents_dir.join(format!("{resume_id}.jsonl"));
    if !jsonl.exists() {
        // List available agent_ids (*.jsonl without the extension) to help the model fix typos
        let mut ids: Vec<String> = std::fs::read_dir(agents_dir)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| {
                let path = entry.path();
                if path.extension().is_some_and(|ext| ext == "jsonl") {
                    path.file_stem().map(|s| s.to_string_lossy().to_string())
                } else {
                    None
                }
            })
            .collect();
        ids.sort();
        let available = if ids.is_empty() {
            "(none)".to_string()
        } else {
            ids.join(", ")
        };
        return Err(format!(
            "Subagent \"{resume_id}\" does not exist. Available: {available}"
        ));
    }
    // Running conflict: a registry entry with the same agent_id and Running -> no parallel continuation allowed
    let running_task = ctx
        .tasks
        .lock()
        .expect("task registry lock")
        .iter()
        .find(|t| {
            t.agent_id.as_deref() == Some(resume_id)
                && matches!(t.status, pig_protocol::TaskStatus::Running)
        })
        .map(|t| t.id.clone());
    if let Some(task_id) = running_task {
        return Err(format!(
            "This subagent is still running (task_id {task_id}); stop it with TaskStop, \
             then resume"
        ));
    }
    let (meta, mut history) = read_agent(&jsonl)?;
    let profile = find_profile(profiles, &meta.profile)?.clone();
    history.push(ChatMsg::user(task.prompt.clone()));
    let child_config = resolve_child_config(ctx, &profile)?;
    persist_msg(&jsonl, history.last().expect("resume user pushed"));
    Ok(assemble_prep(
        resume_id.to_string(),
        profile,
        child_config,
        history,
        jsonl,
        task.description.clone(),
        ctx,
    ))
}

/// Subagent model resolution (strict: failure is an error; without the app config loaded an explicit model is unusable, inherit is unaffected)
fn resolve_child_config(
    ctx: &SwarmPrepCtx<'_>,
    profile: &AgentProfile,
) -> Result<ResolvedModel, String> {
    match ctx.app_config {
        Some(app_config) => resolve_subagent_model(app_config, ctx.parent_config, profile),
        None if profile.model.is_some() => {
            Err("App config not loaded; cannot resolve the subagent's specified model".to_string())
        }
        None => Ok(ctx.parent_config.clone()),
    }
}

/// Assemble the execution prep: tool narrowing (profile list ∩ all - FORBIDDEN;
/// input_image=false additionally removes ReadMediaFile) + MCP inheritance (same rules as
/// run_subagent: full-tool profiles inherit all connected MCP tools, read-only profiles only
/// the readOnlyHint ones) + max_turns default
fn assemble_prep(
    agent_id: String,
    profile: AgentProfile,
    child_config: ResolvedModel,
    history: Vec<ChatMsg>,
    jsonl: PathBuf,
    description: String,
    ctx: &SwarmPrepCtx<'_>,
) -> SwarmChildPrep {
    let web_search = crate::tool::web_search_enabled(child_config.cap_web_search);
    let all_tools: Vec<Box<dyn crate::tool::Tool>> = crate::tool::all()
        .into_iter()
        .filter(|t| web_search || t.name() != "WebSearch")
        .collect();
    let all_names: Vec<String> = all_tools.iter().map(|t| t.name().to_string()).collect();
    let keep = child_tool_set(&profile, &all_names, child_config.input_image);
    let mut tools: Vec<Box<dyn crate::tool::Tool>> = all_tools
        .into_iter()
        .filter(|t| keep.iter().any(|name| name == t.name()))
        .collect();
    let mcp_inherits_all = child_inherits_all_mcp(&keep);
    if let Some(mcp) = ctx.mcp {
        tools.extend(mcp.child_tools(mcp_inherits_all));
    }
    // Skill is supplied to all swarm subagents (same policy as run_subagent: read-only, overriding the profile default)
    tools.push(Box::new(crate::tool::SkillTool::new(ctx.cwd, ctx.data_dir)));
    let schemas: Vec<serde_json::Value> = tools.iter().map(|t| t.schema()).collect();
    SwarmChildPrep {
        max_turns: profile.max_turns.unwrap_or(DEFAULT_MAX_TURNS),
        agent_id,
        cwd: ctx.cwd.to_path_buf(),
        data_dir: ctx.data_dir.to_path_buf(),
        profile,
        child_config,
        tools,
        schemas,
        history,
        jsonl,
        description,
        mcp: ctx.mcp.cloned(),
        mcp_inherits_all,
    }
}

/// Append one line to the subagent context JSONL (failure is non-fatal: log and continue, same policy as rollout.append)
fn persist_line(jsonl: &Path, line: &serde_json::Value) {
    if let Err(error) = append_agent_record(jsonl, line) {
        tracing::error!("failed to persist subagent context: {error}");
    }
}

/// Persist a subagent message: base64 is not persisted (same policy as the main rollout)
fn persist_msg(jsonl: &Path, msg: &ChatMsg) {
    let mut msg = msg.clone();
    msg.images.clear();
    persist_line(jsonl, &serde_json::json!({ "type": "msg", "msg": msg }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use pig_protocol::ApiFormat;

    /// Temp dir: pid + nanos for uniqueness, auto-cleaned on Drop
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let path = std::env::temp_dir().join(format!(
                "pig-swarm-test-{tag}-{}-{nanos}",
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

    fn parent_model() -> ResolvedModel {
        ResolvedModel {
            base_url: "http://parent.local".into(),
            api_key: "parent-key".into(),
            model: "parent-model".into(),
            context_window: 1,
            max_output_tokens: 1,
            api_format: ApiFormat::OpenAiChat,
            reasoning_params: None,
            cap_structured: false,
            cap_strict_tools: false,
            cap_web_search: false,
            web_search_tool: None,
            input_image: true,
            provider_name: "parent provider".into(),
        }
    }

    /// Prep context for an items batch call (cwd/data_dir both under the temp dir)
    fn prep_ctx<'a>(
        tmp: &'a TempDir,
        tasks: &'a crate::task::TaskRegistry,
        parent: &'a ResolvedModel,
    ) -> SwarmPrepCtx<'a> {
        SwarmPrepCtx {
            cwd: &tmp.0,
            data_dir: &tmp.0,
            git_snapshot: None,
            skills_prompt: "",
            agents_prompt: "",
            app_config: None,
            parent_config: parent,
            session_id: "s-test",
            tasks,
            mcp: None,
        }
    }

    /// Fake MCP tool spec for tests
    fn mcp_spec(tool_name: &str, read_only: bool) -> crate::mcp::McpToolSpec {
        crate::mcp::McpToolSpec {
            name: tool_name.to_string(),
            description: None,
            input_schema: serde_json::json!({"type": "object"}),
            annotations: crate::mcp::McpToolAnnotations {
                read_only_hint: Some(read_only),
                ..Default::default()
            },
        }
    }

    // ---------- prepare_swarm_children ----------

    #[test]
    fn prepare_two_items_writes_jsonl() {
        let tmp = TempDir::new("items");
        let tasks = crate::task::TaskRegistry::default();
        let parent = parent_model();
        let plan = crate::tool::parse_swarm_args(&serde_json::json!({
            "prompt_template": "Review {{item}}",
            "items": ["a.rs", "b.rs"]
        }))
        .expect("valid args");
        let mut seq = 0u64;
        let preps = prepare_swarm_children(&prep_ctx(&tmp, &tasks, &parent), &plan, &mut seq)
            .expect("builtin profile should resolve");
        assert_eq!(preps.len(), 2);
        assert_eq!(seq, 2, "each item subtask consumes one seq number");
        let mut agent_ids = Vec::new();
        for prep in &preps {
            let SwarmPrep::Ready(prep) = prep else {
                panic!("item subtasks should all be Ready");
            };
            agent_ids.push(prep.agent_id.clone());
            assert_eq!(prep.profile.name, "general-purpose");
            assert_eq!(
                prep.child_config.model, "parent-model",
                "inherits parent model"
            );
            assert_eq!(prep.history.len(), 2, "system + user");
            assert!(
                prep.history[1]
                    .content
                    .as_deref()
                    .is_some_and(|c| c.contains(".rs")),
                "user message is the expanded prompt"
            );
            let names: Vec<&str> = prep.tools.iter().map(|t| t.name()).collect();
            assert!(names.contains(&"Write"), "general-purpose has all tools");
            for banned in ["Agent", "AgentSwarm", "AskUserQuestion", "EnterPlanMode"] {
                assert!(
                    !names.contains(&banned),
                    "nesting tools must be removed: {banned}"
                );
            }
            // JSONL persisted: meta line + two messages, readable back
            let (meta, history) = read_agent(&prep.jsonl).expect("persisted context reads back");
            assert_eq!(meta.profile, "general-purpose");
            assert_eq!(history.len(), 2);
        }
        assert_ne!(agent_ids[0], agent_ids[1], "agent_id must be unique");
    }

    #[test]
    fn prepare_bad_subagent_type_fails_whole_call() {
        let tmp = TempDir::new("bad-type");
        let tasks = crate::task::TaskRegistry::default();
        let parent = parent_model();
        let plan = crate::tool::parse_swarm_args(&serde_json::json!({
            "prompt_template": "Review {{item}}",
            "items": ["a.rs", "b.rs"],
            "subagent_type": "nonexistent"
        }))
        .expect("arg validation does not check profiles");
        let mut seq = 0u64;
        let err = prepare_swarm_children(&prep_ctx(&tmp, &tasks, &parent), &plan, &mut seq)
            .err()
            .expect("profile error fails the whole call");
        assert!(
            err.contains("nonexistent"),
            "profile error fails the whole call: {err}"
        );
    }

    #[test]
    fn prepare_inherits_mcp_tools_by_profile() {
        let tmp = TempDir::new("mcp-inherit");
        let tasks = crate::task::TaskRegistry::default();
        let parent = parent_model();
        let mcp = std::sync::Arc::new(crate::mcp::McpManager::for_test(
            "srv",
            vec![mcp_spec("read", true), mcp_spec("write", false)],
        ));
        let plan = crate::tool::parse_swarm_args(&serde_json::json!({
            "prompt_template": "Review {{item}}",
            "items": ["a.rs", "b.rs"]
        }))
        .expect("valid args");
        let mut ctx = prep_ctx(&tmp, &tasks, &parent);
        ctx.mcp = Some(&mcp);
        let mut seq = 0u64;
        let preps = prepare_swarm_children(&ctx, &plan, &mut seq).expect("prepare ok");
        let SwarmPrep::Ready(prep) = &preps[0] else {
            panic!("should be Ready");
        };
        let names: Vec<&str> = prep.tools.iter().map(|t| t.name()).collect();
        // general-purpose (full-tool profile): inherits all connected MCP tools, schemas in sync
        assert!(prep.mcp_inherits_all);
        assert!(names.contains(&"mcp__srv__read"), "{names:?}");
        assert!(names.contains(&"mcp__srv__write"), "{names:?}");
        let schema_names: Vec<&str> = prep
            .schemas
            .iter()
            .filter_map(|s| s["function"]["name"].as_str())
            .collect();
        assert!(
            schema_names.contains(&"mcp__srv__write"),
            "{schema_names:?}"
        );

        // explore (read-only profile): inherits only readOnlyHint MCP tools
        let plan = crate::tool::parse_swarm_args(&serde_json::json!({
            "prompt_template": "Investigate {{item}}",
            "items": ["a.rs", "b.rs"],
            "subagent_type": "explore"
        }))
        .expect("valid args");
        let preps = prepare_swarm_children(&ctx, &plan, &mut seq).expect("prepare ok");
        let SwarmPrep::Ready(prep) = &preps[0] else {
            panic!("should be Ready");
        };
        let names: Vec<&str> = prep.tools.iter().map(|t| t.name()).collect();
        assert!(!prep.mcp_inherits_all);
        assert!(names.contains(&"mcp__srv__read"), "{names:?}");
        assert!(!names.contains(&"mcp__srv__write"), "{names:?}");
    }

    #[test]
    fn prepare_resume_conflict_fails_only_that_entry() {
        let tmp = TempDir::new("conflict");
        let tasks = crate::task::TaskRegistry::default();
        let parent = parent_model();
        // Register a Running task with the same agent_id -> resume conflict; the context file
        // only needs to exist (conflict detection happens before reading)
        let agents = agents_dir(&tmp.0.join("sessions"), "s-test");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(agents.join("a1.jsonl"), "").unwrap();
        tasks.lock().unwrap().push(crate::task::TaskEntry {
            id: "b1".into(),
            command: "subagent".into(),
            status: pig_protocol::TaskStatus::Running,
            started_at: 0,
            ended_at: None,
            pid: None,
            output: String::new(),
            spill_path: None,
            cancel: None,
            agent_id: Some("a1".into()),
            foreground: false,
        });
        let plan = crate::tool::parse_swarm_args(&serde_json::json!({
            "prompt_template": "Process {{item}}",
            "items": ["one"],
            "resume_agent_ids": {"a1": "continue"}
        }))
        .expect("mixed args valid");
        let mut seq = 0u64;
        let preps = prepare_swarm_children(&prep_ctx(&tmp, &tasks, &parent), &plan, &mut seq)
            .expect("single failure does not sink the whole call");
        assert_eq!(preps.len(), 2);
        assert!(
            matches!(&preps[0], SwarmPrep::Ready(_)),
            "item subtask unaffected"
        );
        let SwarmPrep::Failed { error, .. } = &preps[1] else {
            panic!("conflicting resume entry should be Failed");
        };
        assert!(error.contains("still running"), "{error}");
    }

    #[test]
    fn prepare_resume_appends_prompt() {
        let tmp = TempDir::new("resume");
        let tasks = crate::task::TaskRegistry::default();
        let parent = parent_model();
        let agents = agents_dir(&tmp.0.join("sessions"), "s-test");
        std::fs::create_dir_all(&agents).unwrap();
        // Valid context: meta + one user message
        append_agent_record(
            &agents.join("a9.jsonl"),
            &serde_json::json!({
                "type": "meta",
                "agent_id": "a9",
                "profile": "general-purpose",
                "description": "old task",
                "model": "m",
                "provider": "p",
                "created_at": 1,
            }),
        )
        .unwrap();
        append_agent_record(
            &agents.join("a9.jsonl"),
            &serde_json::json!({ "type": "msg", "msg": ChatMsg::user("old prompt".to_string()) }),
        )
        .unwrap();
        let plan = crate::tool::parse_swarm_args(&serde_json::json!({
            "prompt_template": "",
            "resume_agent_ids": {"a9": "continue finding callers"}
        }))
        .expect("pure resume");
        let mut seq = 0u64;
        let preps =
            prepare_swarm_children(&prep_ctx(&tmp, &tasks, &parent), &plan, &mut seq).unwrap();
        assert_eq!(seq, 0, "resume consumes no seq number");
        let [SwarmPrep::Ready(prep)] = preps.as_slice() else {
            panic!("single resume should be Ready");
        };
        assert_eq!(prep.agent_id, "a9", "resume reuses the original agent_id");
        assert_eq!(prep.history.len(), 2, "old user + new user");
        assert_eq!(
            prep.history[1].content.as_deref(),
            Some("continue finding callers"),
            "appended prompt enters history"
        );
        let (_, history) = read_agent(&prep.jsonl).unwrap();
        assert_eq!(history.len(), 2, "appended line persisted");
    }

    // ---------- format_swarm_result ----------

    fn child(status: SwarmChildStatus, text: &str) -> SwarmChildResult {
        SwarmChildResult {
            description: "task item".into(),
            agent_id: Some("a1-1".into()),
            status,
            turns: 3,
            result_path: Some(PathBuf::from("/tmp/agents/a1-1.result.md")),
            result_text: text.to_string(),
            queued: false,
            usage: (0, 0, 0),
        }
    }

    #[test]
    fn aggregate_counts_and_queue_note() {
        let mut failed = child(SwarmChildStatus::Failed, "model request failed: boom");
        failed.queued = true;
        let out = format_swarm_result(&[
            child(SwarmChildStatus::Completed, "conclusion one"),
            failed,
            child(SwarmChildStatus::Cancelled, ""),
        ]);
        assert!(
            out.contains("3 subagents total (1 completed / 1 failed / 1 cancelled)"),
            "{out}"
        );
        assert!(
            out.contains("concurrency limit") && out.contains("1 waited in queue"),
            "queue info should be in the header: {out}"
        );
        assert!(out.contains("## 1."), "one section per item");
        assert!(out.contains("Full result: /tmp/agents/a1-1.result.md"));
        assert!(
            out.contains("(stopped, no result)"),
            "cancelled item placeholder"
        );
        assert!(
            out.contains("model request failed: boom"),
            "failed item carries its reason"
        );
    }

    #[test]
    fn aggregate_preview_capped_with_pointer() {
        let big = "x".repeat(SWARM_CHILD_PREVIEW + 500);
        let out = format_swarm_result(&[child(SwarmChildStatus::Completed, &big)]);
        assert!(
            out.contains("[Preview truncated; full result: /tmp/agents/a1-1.result.md]"),
            "{out}"
        );
        let body = out.matches('x').count();
        assert_eq!(body, SWARM_CHILD_PREVIEW, "preview capped at the limit");
    }

    #[test]
    fn aggregate_overflow_degrades_to_pointer_tail() {
        // Many large results blow the total budget: overflowing sections degrade to pointer lines, each item still reaches result.md
        let big = "x".repeat(SWARM_CHILD_PREVIEW);
        let children: Vec<SwarmChildResult> = (0..40)
            .map(|i| {
                let mut c = child(SwarmChildStatus::Completed, &big);
                c.agent_id = Some(format!("a1-{i}"));
                c.result_path = Some(PathBuf::from(format!("/tmp/agents/a1-{i}.result.md")));
                c
            })
            .collect();
        let out = format_swarm_result(&children);
        assert!(
            out.chars().count() <= SWARM_RESULT_BUDGET,
            "total length bounded by budget: {}",
            out.chars().count()
        );
        assert!(out.contains("status and result-file pointers"), "{out}");
        assert!(
            out.contains("a1-39.result.md"),
            "tail item pointers must stay complete"
        );
    }

    #[test]
    fn aggregate_prep_failure_section() {
        let prep_failed = SwarmChildResult {
            description: "resume old agent".into(),
            agent_id: None,
            status: SwarmChildStatus::Failed,
            turns: 0,
            result_path: None,
            result_text: "Subagent \"a9\" does not exist. Available: (none)".into(),
            queued: false,
            usage: (0, 0, 0),
        };
        let out = format_swarm_result(&[prep_failed]);
        assert!(
            out.contains("failed during preparation, not started"),
            "{out}"
        );
        assert!(
            out.contains("does not exist"),
            "error reason inlined: {out}"
        );
    }

    // ---------- format_swarm_receipt (background receipt) ----------

    fn receipt_child(
        description: &str,
        dispatched: Option<(&str, &str)>,
        queued: bool,
        error: Option<&str>,
    ) -> SwarmReceiptChild {
        SwarmReceiptChild {
            description: description.into(),
            agent_id: dispatched.map(|(a, _)| a.into()),
            task_id: dispatched.map(|(_, t)| t.into()),
            queued,
            error: error.map(str::to_string),
        }
    }

    #[test]
    fn receipt_lists_each_child_with_ids_and_status() {
        let out = format_swarm_receipt(&[
            receipt_child("Review a.rs", Some(("a1-1", "b1")), false, None),
            receipt_child("Review b.rs", Some(("a1-2", "b2")), true, None),
            receipt_child(
                "resume old agent",
                None,
                false,
                Some("Subagent \"a9\" does not exist"),
            ),
        ]);
        assert!(out.contains("3 subagents total"), "{out}");
        assert!(
            out.contains("queue for a free slot"),
            "queue info in the header: {out}"
        );
        assert!(
            out.contains("1 failed during preparation"),
            "failure count in the header: {out}"
        );
        assert!(
            out.contains("<task-notification>"),
            "notification semantics stated: {out}"
        );
        assert!(out.contains("do not poll"), "{out}");
        assert!(out.contains("1. Review a.rs"), "each item listed: {out}");
        assert!(
            out.contains("agent_id: a1-1 · task_id: b1 · status: running"),
            "{out}"
        );
        assert!(
            out.contains("agent_id: a1-2 · task_id: b2 · status: queued"),
            "{out}"
        );
        assert!(
            out.contains(
                "status: failed (failed during preparation, not started): Subagent \"a9\" does not exist"
            ),
            "prep-failed item carries its reason: {out}"
        );
    }

    #[test]
    fn receipt_all_running_no_queue_no_failure_notes() {
        let out = format_swarm_receipt(&[
            receipt_child("task one", Some(("a1-1", "b1")), false, None),
            receipt_child("task two", Some(("a1-2", "b2")), false, None),
        ]);
        assert!(
            out.contains("2 subagents total."),
            "header clean when no notes: {out}"
        );
        assert!(!out.contains("queue"), "{out}");
        assert!(!out.contains("failed during preparation"), "{out}");
    }
}
