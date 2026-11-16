//! Aggregation side of AgentSwarm batch-parallel subagents: the child prep
//! results come from the shared `child` builder (the same profile/model/tool/
//! MCP/media/JSONL pipeline as a single Agent), and this module owns the
//! aggregated result and background receipt formatting. Argument expansion/
//! validation lives in tool::parse_swarm_args (a pure function at the schema
//! layer); concurrent driving goes through the session layer's one managed
//! child driver; the global concurrency slots live in task.rs.

use super::*;

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
    if child.status != SwarmChildStatus::Completed
        && let Some(agent_id) = &child.agent_id
    {
        section.push_str(&format!(
            "\nresume_hint: Agent(resume=\"{agent_id}\", prompt=\"...\")"
        ));
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

/// Immediate receipt of a background swarm: terminal results flow through the
/// session's notification lifecycle; the result file remains separately readable.
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
         Each item's terminal notification is delivered automatically; use TaskWait to wait \
         and TaskOutput for progress rather than repeatedly polling. The full result is in \
         the file the notification points to (read it with Read).\n\
         Use TaskList to list tasks, TaskStop to stop one, and \
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{RuntimeConfig, WorkspaceTrust};
    use pig_protocol::ApiFormat;
    use pig_provider::{ChatMsg, ResolvedModel};

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
            input_pdf: false,
            anthropic_cache_control: false,
            replay_native_search: false,
            prompt_cache_key: None,
            provider_name: "parent provider".into(),
            tool_settings: Default::default(),
        }
    }

    /// Prep context for an items batch call (cwd/data_dir both under the temp dir)
    fn prep_ctx<'a>(
        tmp: &'a TempDir,
        state: &'a crate::task::SessionToolState,
        parent: &'a ResolvedModel,
    ) -> ChildPrepCtx<'a> {
        ChildPrepCtx {
            cwd: &tmp.0,
            data_dir: &tmp.0,
            git_snapshot: None,
            skills_prompt: "",
            agents_prompt: "",
            runtime: None,
            parent_config: parent,
            session_id: "s-test",
            parent_agent_id: "s-test",
            parent_delegation_depth: 0,
            max_delegation_depth: 1,
            max_agents_per_session: 64,
            allowed_subagents: None,
            child_cwd: None,
            worktree: false,
            parent_permissions: pig_protocol::PermissionSettings::default(),
            parent_history: &[],
            profiles_summary: "",
            state,
            mcp: None,
            plugin_tools: crate::plugins::PluginToolset::new(
                &crate::plugins::PluginRegistry::shared(tmp.0.clone()),
                &WorkspaceTrust::untrusted(&tmp.0),
                None,
                "s-test",
            ),
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
        let state = crate::task::SessionToolState::for_test();
        let parent = parent_model();
        let plan = crate::tool::parse_swarm_args(&serde_json::json!({
            "prompt_template": "Review {{item}}",
            "items": ["a.rs", "b.rs"]
        }))
        .expect("valid args");
        let mut seq = 0u64;
        let preps = prepare_swarm_children(&prep_ctx(&tmp, &state, &parent), &plan, &mut seq)
            .expect("builtin profile should resolve");
        assert_eq!(preps.len(), 2);
        assert_eq!(seq, 2, "each item subtask consumes one seq number");
        let mut agent_ids = Vec::new();
        for prep in &preps {
            let SwarmPrep::Ready(prep) = prep else {
                panic!("item subtasks should all be Ready");
            };
            agent_ids.push(prep.agent_id.clone());
            assert_eq!(prep.policy.profile.name, "general-purpose");
            assert_eq!(
                prep.policy.model.model, "parent-model",
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
            assert!(
                names.contains(&"TodoList"),
                "TodoList is a normal child tool with its own local state"
            );
            for banned in ["Agent", "AgentSwarm", "AskUserQuestion", "EnterPlanMode"] {
                assert!(
                    !names.contains(&banned),
                    "nesting tools must be removed: {banned}"
                );
            }
            // JSONL persisted: meta line + two messages, readable back
            let (meta, history) = read_agent(&prep.jsonl).expect("persisted context reads back");
            assert_eq!(meta.profile, "general-purpose");
            assert_eq!(meta.parent_session_id, "s-test");
            assert_eq!(meta.delegation_depth, 1);
            assert_eq!(history.len(), 2);
        }
        assert_ne!(agent_ids[0], agent_ids[1], "agent_id must be unique");
    }

    #[test]
    fn prepare_nested_delegation_respects_depth_and_profile() {
        let tmp = TempDir::new("nested-depth");
        let state = crate::task::SessionToolState::for_test();
        let parent = parent_model();
        let plan = crate::tool::parse_swarm_args(&serde_json::json!({
            "prompt_template": "Review {{item}}",
            "items": ["a.rs", "b.rs"]
        }))
        .expect("valid args");
        let mut ctx = prep_ctx(&tmp, &state, &parent);
        ctx.max_delegation_depth = 2;
        ctx.parent_permissions = pig_protocol::ExecMode::Full.permissions();
        let mut seq = 0u64;

        let preps = prepare_swarm_children(&ctx, &plan, &mut seq).expect("depth one child");
        let SwarmPrep::Ready(prep) = &preps[0] else {
            panic!("child should be Ready");
        };
        assert_eq!(prep.policy.delegation_depth, 1);
        assert_eq!(
            prep.policy.permissions,
            pig_protocol::ExecMode::Full.permissions(),
            "the policy carries the parent's activation-time permission snapshot"
        );
        let names: Vec<&str> = prep.tools.iter().map(|tool| tool.name()).collect();
        for expected in ["Agent", "AgentSwarm", "SendMessage"] {
            assert!(names.contains(&expected), "missing {expected}: {names:?}");
        }

        ctx.parent_delegation_depth = 1;
        let preps = prepare_swarm_children(&ctx, &plan, &mut seq).expect("depth two child");
        let SwarmPrep::Ready(prep) = &preps[0] else {
            panic!("child should be Ready");
        };
        assert_eq!(prep.policy.delegation_depth, 2);
        assert!(
            !prep.tools.iter().any(|tool| tool.name() == "Agent"),
            "the depth cap removes further delegation"
        );

        ctx.max_delegation_depth = 1;
        let error = prepare_swarm_children(&ctx, &plan, &mut seq)
            .err()
            .expect("nested child above the cap is rejected");
        assert!(error.contains("Delegation depth 2"), "{error}");
        assert!(error.contains("max_delegation_depth=1"), "{error}");

        ctx.parent_delegation_depth = 0;
        ctx.max_delegation_depth = 2;
        let plan = crate::tool::parse_swarm_args(&serde_json::json!({
            "prompt_template": "Investigate {{item}}",
            "items": ["a.rs", "b.rs"],
            "subagent_type": "explore"
        }))
        .expect("explore args");
        let preps = prepare_swarm_children(&ctx, &plan, &mut seq).expect("read-only child");
        let SwarmPrep::Ready(prep) = &preps[0] else {
            panic!("child should be Ready");
        };
        assert!(
            !prep.tools.iter().any(|tool| tool.name() == "Agent"),
            "a read-only profile cannot acquire delegation through the global depth setting"
        );
    }

    #[test]
    fn child_budget_counts_batch_reservations_but_not_resume() {
        let tmp = TempDir::new("child-budget");
        let state = crate::task::SessionToolState::for_test();
        let parent = parent_model();
        let plan = crate::tool::parse_swarm_args(&serde_json::json!({
            "prompt_template": "Review {{item}}",
            "items": ["a.rs", "b.rs"]
        }))
        .expect("valid args");
        let mut ctx = prep_ctx(&tmp, &state, &parent);
        ctx.max_agents_per_session = 1;
        let mut seq = 0u64;
        let error = prepare_swarm_children(&ctx, &plan, &mut seq)
            .err()
            .expect("the second batch reservation exceeds the budget");
        assert!(error.contains("max_agents_per_session=1"), "{error}");

        let profiles = load_profiles_with_trust(&tmp.0, &tmp.0, &WorkspaceTrust::untrusted(&tmp.0));
        let prep = prepare_fresh_child(
            &ctx,
            find_profile(&profiles, "general-purpose").unwrap().clone(),
            parent.clone(),
            "one child",
            "one child",
            &mut seq,
        )
        .expect("first child");
        let agent_id = prep.agent_id.clone();
        drop(prep);
        prepare_resume_child(&ctx, &profiles, &agent_id, "continue", "one child")
            .expect("resume does not consume another agent slot");
    }

    #[test]
    fn parent_profile_allowlist_applies_to_fresh_and_resume() {
        let tmp = TempDir::new("child-allowlist");
        let state = crate::task::SessionToolState::for_test();
        let parent = parent_model();
        let profiles = load_profiles_with_trust(&tmp.0, &tmp.0, &WorkspaceTrust::untrusted(&tmp.0));
        let allowed = vec!["explore".to_string()];
        let mut ctx = prep_ctx(&tmp, &state, &parent);
        ctx.allowed_subagents = Some(&allowed);
        let mut seq = 0u64;
        let error = prepare_fresh_child(
            &ctx,
            find_profile(&profiles, "general-purpose").unwrap().clone(),
            parent.clone(),
            "not allowed",
            "not allowed",
            &mut seq,
        )
        .err()
        .expect("parent profile rejects the type")
        .message();
        assert!(error.contains("not allowed"), "{error}");

        let prep = prepare_fresh_child(
            &ctx,
            find_profile(&profiles, "explore").unwrap().clone(),
            parent.clone(),
            "allowed",
            "allowed",
            &mut seq,
        )
        .expect("allowed child");
        let agent_id = prep.agent_id.clone();
        drop(prep);
        ctx.allowed_subagents = Some(&[]);
        let error = prepare_resume_child(&ctx, &profiles, &agent_id, "continue", "allowed")
            .err()
            .expect("resume cannot bypass the allowlist")
            .message();
        assert!(error.contains("not allowed"), "{error}");
    }

    #[test]
    fn worktree_child_creates_an_isolated_cwd_and_resume_reuses_it() {
        let tmp = TempDir::new("child-worktree");
        let workspace = tmp.0.join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let git = |args: &[&str]| {
            let mut command = std::process::Command::new("git");
            command.arg("-C").arg(&workspace).args(args);
            crate::NoConsoleExt::no_console(&mut command);
            let status = command.status().expect("git process");
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init"]);
        std::fs::write(workspace.join("tracked.txt"), "tracked").expect("tracked file");
        git(&["add", "tracked.txt"]);
        git(&[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-m",
            "init",
        ]);
        let state = crate::task::SessionToolState::for_test();
        let parent = parent_model();
        let profiles = load_profiles_with_trust(&tmp.0, &tmp.0, &WorkspaceTrust::untrusted(&tmp.0));
        let mut ctx = prep_ctx(&tmp, &state, &parent);
        ctx.cwd = &workspace;
        ctx.worktree = true;
        let prep = prepare_fresh_child(
            &ctx,
            find_profile(&profiles, "general-purpose").unwrap().clone(),
            parent.clone(),
            "worktree task",
            "worktree child",
            &mut 0,
        )
        .expect("worktree child");
        let worktree = prep.cwd.clone();
        assert_ne!(worktree, workspace);
        assert!(worktree.join("tracked.txt").is_file());
        let (meta, _) = read_agent(&prep.jsonl).expect("worktree metadata");
        assert_eq!(meta.cwd, worktree);
        drop(prep);
        let resumed = prepare_resume_child(
            &ctx,
            &profiles,
            &meta.agent_id,
            "continue in the existing worktree",
            "worktree child",
        )
        .expect("resume worktree");
        assert_eq!(resumed.cwd, worktree);
    }

    #[test]
    fn prepare_fork_copies_an_independent_replayable_snapshot() {
        let tmp = TempDir::new("fork-context");
        let state = crate::task::SessionToolState::for_test();
        let parent = parent_model();
        let profiles = load_profiles_with_trust(&tmp.0, &tmp.0, &WorkspaceTrust::untrusted(&tmp.0));
        let profile = find_profile(&profiles, "general-purpose")
            .expect("general profile")
            .clone();
        let answered = pig_provider::ToolCall {
            id: "call-answered".into(),
            name: "Read".into(),
            arguments: "{\"path\":\"old.txt\"}".into(),
        };
        let pending = pig_provider::ToolCall {
            id: "call-pending".into(),
            name: "Agent".into(),
            arguments: "{}".into(),
        };
        let parent_history = vec![
            ChatMsg::system("parent system".into()),
            ChatMsg::user("inherited request".into()),
            ChatMsg::assistant(String::new(), vec![answered], None),
            ChatMsg::tool_result("call-answered", "old result".to_string()),
            ChatMsg::assistant("current parent work".into(), vec![pending], None),
        ];
        let mut ctx = prep_ctx(&tmp, &state, &parent);
        ctx.parent_history = &parent_history;
        let mut seq = 0u64;
        let prep = prepare_fork_child(
            &ctx,
            profile,
            parent.clone(),
            "new child task",
            "fork child",
            &mut seq,
        )
        .expect("fork preparation");
        assert_eq!(
            prep.policy.context_mode,
            crate::rollout::ChildContextMode::Fork
        );
        assert_eq!(prep.policy.inherited_record_count, Some(4));
        assert_eq!(prep.history.len(), 7);
        assert_ne!(prep.history[0].content.as_deref(), Some("parent system"));
        assert!(
            prep.history[1]
                .content
                .as_deref()
                .is_some_and(|text| text == "inherited request")
        );
        assert!(
            prep.history[4]
                .tool_calls
                .as_ref()
                .is_none_or(|calls| calls.is_empty()),
            "the parent's unfinished call is not replayed into the child"
        );
        assert_eq!(prep.history[5].role, "user");
        assert_eq!(prep.history[6].content.as_deref(), Some("new child task"));

        let (meta, history) = read_agent(&prep.jsonl).expect("read fork context");
        assert_eq!(meta.context_mode, crate::rollout::ChildContextMode::Fork);
        assert_eq!(meta.inherited_record_count, Some(4));
        assert_eq!(history.len(), 7, "the copied prefix replays in full");
        let jsonl = prep.jsonl.clone();
        drop(prep);
        let resumed = prepare_resume_child(
            &ctx,
            &profiles,
            &meta.agent_id,
            "continue fork",
            "fork child",
        )
        .expect("resume fork");
        assert_eq!(
            resumed.policy.context_mode,
            crate::rollout::ChildContextMode::Fork
        );
        assert_eq!(resumed.policy.inherited_record_count, Some(4));
        assert_eq!(
            resumed
                .history
                .last()
                .and_then(|message| message.content.as_deref()),
            Some("continue fork")
        );
        let (_, persisted) = read_agent(&jsonl).expect("read resumed fork");
        assert_eq!(persisted.len(), 8, "resume appends only to the child JSONL");
    }

    #[test]
    fn prepare_bad_subagent_type_fails_whole_call() {
        let tmp = TempDir::new("bad-type");
        let state = crate::task::SessionToolState::for_test();
        let parent = parent_model();
        let plan = crate::tool::parse_swarm_args(&serde_json::json!({
            "prompt_template": "Review {{item}}",
            "items": ["a.rs", "b.rs"],
            "subagent_type": "nonexistent"
        }))
        .expect("arg validation does not check profiles");
        let mut seq = 0u64;
        let err = prepare_swarm_children(&prep_ctx(&tmp, &state, &parent), &plan, &mut seq)
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
        let state = crate::task::SessionToolState::for_test();
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
        let mut ctx = prep_ctx(&tmp, &state, &parent);
        ctx.mcp = Some(&mcp);
        let mut seq = 0u64;
        let preps = prepare_swarm_children(&ctx, &plan, &mut seq).expect("prepare ok");
        let SwarmPrep::Ready(prep) = &preps[0] else {
            panic!("should be Ready");
        };
        let names: Vec<&str> = prep.tools.iter().map(|t| t.name()).collect();
        // general-purpose (full-tool profile): inherits all connected MCP tools, schemas in sync
        assert!(prep.policy.mcp_inherits_all);
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
        assert!(!prep.policy.mcp_inherits_all);
        assert!(names.contains(&"mcp__srv__read"), "{names:?}");
        assert!(!names.contains(&"mcp__srv__write"), "{names:?}");
    }

    /// MCP sanitization can collapse two original tool names to one; the child
    /// must reject the batch instead of routing every call to the first match.
    #[test]
    fn prepare_rejects_mcp_name_collisions() {
        let tmp = TempDir::new("mcp-collision");
        let state = crate::task::SessionToolState::for_test();
        let parent = parent_model();
        let mcp = std::sync::Arc::new(crate::mcp::McpManager::for_test(
            "srv",
            vec![mcp_spec("a b", true), mcp_spec("a_b", true)],
        ));
        let plan = crate::tool::parse_swarm_args(&serde_json::json!({
            "prompt_template": "Review {{item}}",
            "items": ["a.rs", "b.rs"]
        }))
        .expect("valid args");
        let mut ctx = prep_ctx(&tmp, &state, &parent);
        ctx.mcp = Some(&mcp);
        let mut seq = 0u64;
        let error = prepare_swarm_children(&ctx, &plan, &mut seq)
            .err()
            .expect("a collision must fail the batch");
        assert!(
            error.contains("MCP tool name collision"),
            "the error explains the ambiguous name: {error}"
        );
    }

    /// Project profiles are repository content: runtime activation uses the
    /// same workspace-trust decision as hooks instead of the settings listing.
    /// A child whose context file cannot be created fails its own item (with
    /// the agent id) instead of starting a live child the JSONL does not
    /// contain; the writer claim is released.
    #[cfg(unix)]
    #[test]
    fn prepare_context_write_failure_fails_only_that_item() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = TempDir::new("prep-write-fail");
        let state = crate::task::SessionToolState::for_test();
        let parent = parent_model();
        // Pre-create the agent directory without write permission so the
        // child's meta line cannot be created.
        let agents = tmp.0.join("sessions/s-test.agents");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::set_permissions(&agents, std::fs::Permissions::from_mode(0o555)).unwrap();
        let plan = crate::tool::parse_swarm_args(&serde_json::json!({
            "prompt_template": "Review {{item}}",
            "items": ["a.rs", "b.rs"]
        }))
        .expect("valid args");
        let mut seq = 0u64;
        let preps = prepare_swarm_children(&prep_ctx(&tmp, &state, &parent), &plan, &mut seq)
            .expect("per-item failures do not sink the batch");
        std::fs::set_permissions(&agents, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(preps.len(), 2);
        for prep in &preps {
            let SwarmPrep::Failed { error, .. } = prep else {
                panic!("a context write failure must fail the item");
            };
            assert!(
                error.contains("could not persist its context"),
                "the failure names persistence: {error}"
            );
            assert!(
                error.contains("Subagent a") && error.contains("cannot be resumed"),
                "the failure carries the generated agent id: {error}"
            );
        }
        assert!(
            state.agent_writers.lock().expect("writers").is_empty(),
            "every failed prep must release its writer claim"
        );
    }

    /// A trusted project skill is visible in the swarm child's prompt and must
    /// be loadable through its Skill tool; an untrusted workspace denies it.
    #[tokio::test]
    async fn swarm_skill_tool_honors_workspace_trust() {
        let tmp = TempDir::new("skill-trust");
        std::fs::create_dir_all(tmp.0.join(".pigcode/skills/demo")).unwrap();
        std::fs::write(
            tmp.0.join(".pigcode/skills/demo/SKILL.md"),
            "---\nname: demo\ndescription: project skill\n---\nPROJECT_SKILL_BODY\n",
        )
        .unwrap();
        let state = crate::task::SessionToolState::for_test();
        let parent = parent_model();
        let plan = crate::tool::parse_swarm_args(&serde_json::json!({
            "prompt_template": "Review {{item}}",
            "items": ["a.rs", "b.rs"]
        }))
        .expect("valid args");
        let mut seq = 0u64;

        let untrusted = prepare_swarm_children(&prep_ctx(&tmp, &state, &parent), &plan, &mut seq)
            .expect("untrusted prep");
        let SwarmPrep::Ready(untrusted) = &untrusted[0] else {
            panic!("should be Ready");
        };
        assert!(!untrusted.policy.trusted);
        let (output, is_error) = run_swarm_skill(untrusted).await;
        assert!(
            is_error,
            "an untrusted project skill must stay denied: {output}"
        );

        let mut config = pig_protocol::AppConfig::default();
        config.hooks.trusted_projects = vec![
            tmp.0
                .canonicalize()
                .expect("canonical workspace")
                .display()
                .to_string(),
        ];
        let runtime = RuntimeConfig::from_snapshot(
            config,
            &tmp.0,
            std::sync::Arc::new(crate::credentials::CredentialManager::mock()),
        );
        let mut ctx = prep_ctx(&tmp, &state, &parent);
        ctx.runtime = Some(&runtime);
        let trusted = prepare_swarm_children(&ctx, &plan, &mut seq).expect("trusted prep");
        let SwarmPrep::Ready(trusted) = &trusted[0] else {
            panic!("should be Ready");
        };
        assert!(trusted.policy.trusted);
        let (output, is_error) = run_swarm_skill(trusted).await;
        assert!(!is_error, "a trusted project skill must load: {output}");
        assert!(output.contains("PROJECT_SKILL_BODY"), "{output}");
    }

    async fn run_swarm_skill(prep: &ChildPrep) -> (String, bool) {
        let state = crate::task::SessionToolState::for_test();
        let mut tracker = crate::tool::ChangeTracker::default();
        let call = pig_provider::ToolCall {
            id: "call-skill".into(),
            name: "Skill".into(),
            arguments: serde_json::json!({"skill": "demo"}).to_string(),
        };
        let ctx = crate::tool::ToolContext {
            cwd: &prep.cwd,
            tracker: &mut tracker,
            state: &state,
            fs_grant: None,
            sandbox_override: Default::default(),
            sandbox_profile: crate::task::sandbox_profile_for(
                pig_protocol::PermissionSettings::default(),
                false,
            ),
            additional_permissions: Default::default(),
        };
        let (output, is_error, ..) = crate::tool::execute_with_extra(&call, ctx, &prep.tools).await;
        (output, is_error)
    }

    #[test]
    fn prepare_project_profiles_require_trust() {
        let tmp = TempDir::new("profiles-trust");
        let state = crate::task::SessionToolState::for_test();
        let parent = parent_model();
        std::fs::create_dir_all(tmp.0.join(".pigcode/agents")).unwrap();
        std::fs::write(
            tmp.0.join(".pigcode/agents/project.md"),
            "---\nname: project-agent\ndescription: project profile\n---\nbody",
        )
        .unwrap();
        let plan = crate::tool::parse_swarm_args(&serde_json::json!({
            "prompt_template": "Review {{item}}",
            "items": ["a.rs", "b.rs"],
            "subagent_type": "project-agent"
        }))
        .expect("valid args");
        let mut seq = 0u64;
        let untrusted = prepare_swarm_children(&prep_ctx(&tmp, &state, &parent), &plan, &mut seq)
            .err()
            .expect("an untrusted project profile must not load");
        assert!(untrusted.contains("No subagent found"), "{untrusted}");

        let mut config = pig_protocol::AppConfig::default();
        config.hooks.trusted_projects = vec![
            tmp.0
                .canonicalize()
                .expect("canonical workspace")
                .display()
                .to_string(),
        ];
        let runtime = RuntimeConfig::from_snapshot(
            config,
            &tmp.0,
            std::sync::Arc::new(crate::credentials::CredentialManager::mock()),
        );
        let mut ctx = prep_ctx(&tmp, &state, &parent);
        ctx.runtime = Some(&runtime);
        let preps = prepare_swarm_children(&ctx, &plan, &mut seq).expect("trusted profile loads");
        let SwarmPrep::Ready(prep) = &preps[0] else {
            panic!("trusted project profile should be Ready");
        };
        assert_eq!(prep.policy.profile.name, "project-agent");
    }

    #[test]
    fn prepare_resume_conflict_fails_only_that_entry() {
        let tmp = TempDir::new("conflict");
        let state = crate::task::SessionToolState::for_test();
        let parent = parent_model();
        // A Running task with the same agent_id conflicts before any read.
        let task_id = crate::task::register_agent_task(
            &state,
            "subagent explore: running".to_string(),
            tokio_util::sync::CancellationToken::new(),
            "a1-1".to_string(),
        );
        let plan = crate::tool::parse_swarm_args(&serde_json::json!({
            "prompt_template": "Process {{item}}",
            "items": ["one"],
            "resume_agent_ids": {"a1-1": "continue"}
        }))
        .expect("mixed args valid");
        let mut seq = 0u64;
        let preps = prepare_swarm_children(&prep_ctx(&tmp, &state, &parent), &plan, &mut seq)
            .expect("single failure does not sink the whole call");
        assert_eq!(preps.len(), 2);
        assert!(
            matches!(&preps[0], SwarmPrep::Ready(_)),
            "item subtask unaffected"
        );
        let SwarmPrep::Failed { error, .. } = &preps[1] else {
            panic!("conflicting resume entry should be Failed");
        };
        assert!(
            error.contains("still running") && error.contains(&task_id),
            "the refusal names the running task: {error}"
        );
    }

    #[test]
    fn prepare_resume_preserves_depth_and_enforces_limit() {
        let tmp = TempDir::new("resume");
        let state = crate::task::SessionToolState::for_test();
        let parent = parent_model();
        let agents = agents_dir(&tmp.0.join("sessions"), "s-test");
        std::fs::create_dir_all(&agents).unwrap();
        // Valid context: canonical meta + one user message
        let path = agents.join("a9-1.jsonl");
        append_agent_conversation_record(
            &path,
            &crate::rollout::RolloutRecord::subagent_meta(
                "a9-1".into(),
                "s-test".into(),
                tmp.0.clone(),
                "old task".into(),
                "general-purpose".into(),
                "p".into(),
                "m".into(),
                2,
                1,
            ),
        )
        .unwrap();
        let mut user = ChatMsg::user("old prompt".to_string());
        append_agent_message(&path, &mut user, 128_000).unwrap();
        let plan = crate::tool::parse_swarm_args(&serde_json::json!({
            "prompt_template": "",
            "resume_agent_ids": {"a9-1": "continue finding callers"}
        }))
        .expect("pure resume");
        let mut seq = 0u64;
        let mut ctx = prep_ctx(&tmp, &state, &parent);
        ctx.max_delegation_depth = 2;
        let preps = prepare_swarm_children(&ctx, &plan, &mut seq).unwrap();
        assert_eq!(seq, 0, "resume consumes no seq number");
        let [SwarmPrep::Ready(prep)] = preps.as_slice() else {
            panic!("single resume should be Ready");
        };
        assert_eq!(prep.agent_id, "a9-1", "resume reuses the original agent_id");
        assert_eq!(
            prep.policy.delegation_depth, 2,
            "persisted depth is a monotonic lower bound, not parent depth plus one"
        );
        assert_eq!(prep.history.len(), 2, "old user + new user");
        assert_eq!(
            prep.history[1].content.as_deref(),
            Some("continue finding callers"),
            "appended prompt enters history"
        );
        let (_, history) = read_agent(&prep.jsonl).unwrap();
        assert_eq!(history.len(), 2, "appended line persisted");

        drop(preps);
        ctx.max_delegation_depth = 1;
        let rejected = prepare_swarm_children(&ctx, &plan, &mut seq).unwrap();
        let SwarmPrep::Failed { error, .. } = &rejected[0] else {
            panic!("a persisted depth above the current limit must be rejected");
        };
        assert!(error.contains("Delegation depth 2"), "{error}");
        let (_, history) = read_agent(&path).unwrap();
        assert_eq!(
            history.len(),
            2,
            "a refused resume does not append another prompt"
        );
    }

    #[test]
    fn prepare_resume_projects_pdf_out_when_child_lacks_input() {
        let tmp = TempDir::new("resume-pdf");
        let state = crate::task::SessionToolState::for_test();
        let parent = parent_model();
        let agents = agents_dir(&tmp.0.join("sessions"), "s-test");
        std::fs::create_dir_all(&agents).unwrap();
        let path = agents.join("a9-1.jsonl");
        append_agent_conversation_record(
            &path,
            &crate::rollout::RolloutRecord::subagent_meta(
                "a9-1".into(),
                "s-test".into(),
                tmp.0.clone(),
                "pdf task".into(),
                "general-purpose".into(),
                "p".into(),
                "m".into(),
                1,
                1,
            ),
        )
        .unwrap();
        let mut system = ChatMsg::system("subagent".into());
        append_agent_message(&path, &mut system, 128_000).unwrap();
        let call = pig_provider::ToolCall {
            id: "call_pdf".into(),
            name: "ReadMediaFile".into(),
            arguments: "{}".into(),
        };
        let mut assistant = ChatMsg::assistant(String::new(), vec![call.clone()], None);
        append_agent_message_meta(
            &path,
            &mut assistant,
            128_000,
            crate::agent::AgentMsgMeta {
                tool_execution_ids: vec!["execution-pdf".into()],
                ..Default::default()
            },
        )
        .unwrap();
        let mut tool = ChatMsg::tool_result("call_pdf", "Read PDF doc.pdf".into());
        tool.documents = vec![pig_provider::ChatDocument {
            media_type: "application/pdf".into(),
            data_base64: crate::tool::base64_encode(b"%PDF-1.4\n%%EOF\n"),
            filename: "doc.pdf".into(),
            label: Some("doc.pdf".into()),
            token_estimate: None,
        }];
        append_agent_tool_result(
            &path,
            &call,
            "execution-pdf",
            &mut tool,
            128_000,
            crate::agent::AgentMsgMeta::default(),
        )
        .unwrap();

        let plan = crate::tool::parse_swarm_args(&serde_json::json!({
            "prompt_template": "",
            "resume_agent_ids": {"a9-1": "continue"}
        }))
        .expect("pure resume");
        let mut seq = 0u64;
        let preps =
            prepare_swarm_children(&prep_ctx(&tmp, &state, &parent), &plan, &mut seq).unwrap();
        let [SwarmPrep::Ready(prep)] = preps.as_slice() else {
            panic!("single resume should be Ready");
        };
        let tool_msg = prep
            .history
            .iter()
            .find(|message| message.role == "tool")
            .expect("seeded tool result");
        assert!(tool_msg.documents.is_empty());
        assert!(
            tool_msg
                .content
                .as_deref()
                .is_some_and(|text| text.contains("does not support PDF input")),
            "{:?}",
            tool_msg.content
        );
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains("\"documents\""),
            "the durable ref is not rewritten: {raw}"
        );
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
        assert!(
            out.contains("resume_hint: Agent(resume=\"a1-1\", prompt=\"...\")"),
            "failed/cancelled receipts keep a resume path: {out}"
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
            out.contains("terminal notification") && out.contains("TaskWait"),
            "notification semantics stated: {out}"
        );
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
