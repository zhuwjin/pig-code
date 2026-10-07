use super::*;

pub(crate) struct TodoListTool;

impl TodoListTool {
    fn render(todos: &[TodoItem]) -> String {
        if todos.is_empty() {
            return "The todo list is empty".to_string();
        }
        todos
            .iter()
            .enumerate()
            .map(|(i, item)| {
                let status = match item.status {
                    TodoStatus::Pending => "pending",
                    TodoStatus::InProgress => "in_progress",
                    TodoStatus::Done => "done",
                };
                format!("{}. [{}] {}", i + 1, status, item.content)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl Tool for TodoListTool {
    fn name(&self) -> &'static str {
        "TodoList"
    }

    fn read_only(&self) -> bool {
        true
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "TodoList",
                "description": "Manage the session todo list. Break multi-step work into a list at the start and update progress as you go; omit the todos argument to read the current list, or provide todos to replace the whole list (not incremental). At most one item should be in_progress at a time.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "todos": {
                            "type": "array",
                            "description": "The complete new todo list (replaces the whole list)",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "content": { "type": "string", "description": "Todo item content" },
                                    "status": { "type": "string", "enum": ["pending", "in_progress", "done"] }
                                },
                                "required": ["content", "status"]
                            }
                        }
                    }
                }
            }
        })
    }

    fn execute<'a>(
        &'a self,
        args: serde_json::Value,
        ctx: ToolContext<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<ToolEffect, String>> + Send + 'a>> {
        Box::pin(async move {
            match args.get("todos") {
                None => {
                    let todos = ctx.state.todos.lock().map_err(|e| e.to_string())?;
                    Ok(ToolEffect::plain(Self::render(&todos)))
                }
                Some(value) => {
                    let new: Vec<TodoItem> =
                        serde_json::from_value(value.clone()).map_err(|e| {
                            format!("Invalid todos: {e} (status must be pending/in_progress/done)")
                        })?;
                    let mut todos = ctx.state.todos.lock().map_err(|e| e.to_string())?;
                    *todos = new;
                    Ok(ToolEffect::plain(Self::render(&todos)))
                }
            }
        })
    }
}

fn task_status_label(status: pig_protocol::TaskStatus) -> String {
    match status {
        pig_protocol::TaskStatus::Running => "running".to_string(),
        pig_protocol::TaskStatus::Exited(code) => format!("exited({code})"),
        pig_protocol::TaskStatus::Killed => "stopped".to_string(),
    }
}

fn task_duration_label(started_at: u64, ended_at: Option<u64>) -> String {
    let secs = ended_at
        .unwrap_or_else(crate::rollout::now_secs)
        .saturating_sub(started_at);
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m", secs / 60)
    }
}

pub(crate) struct TaskList;

impl Tool for TaskList {
    fn name(&self) -> &'static str {
        "TaskList"
    }

    fn read_only(&self) -> bool {
        true
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "TaskList",
                "description": "List the background Bash tasks of this session (id, status, command, elapsed time).",
                "parameters": {
                    "type": "object",
                    "properties": {}
                }
            }
        })
    }

    fn execute<'a>(
        &'a self,
        _args: serde_json::Value,
        ctx: ToolContext<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<ToolEffect, String>> + Send + 'a>> {
        Box::pin(async move {
            let tasks = ctx.state.tasks.lock().map_err(|e| e.to_string())?;
            if tasks.is_empty() {
                return Ok(ToolEffect::plain("No background tasks".to_string()));
            }
            let out = tasks
                .iter()
                .map(|entry| {
                    format!(
                        "{} [{}] {} ({})",
                        entry.id,
                        task_status_label(entry.status),
                        entry.command,
                        task_duration_label(entry.started_at, entry.ended_at)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            Ok(ToolEffect::plain(out))
        })
    }
}

pub(crate) struct TaskOutput;

impl Tool for TaskOutput {
    fn name(&self) -> &'static str {
        "TaskOutput"
    }

    fn read_only(&self) -> bool {
        true
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "TaskOutput",
                "description": "Read a background Bash task's output (tail excerpt).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "task_id": { "type": "string", "description": "Background task id (b1, b2, ...)" }
                    },
                    "required": ["task_id"]
                }
            }
        })
    }

    fn execute<'a>(
        &'a self,
        args: serde_json::Value,
        ctx: ToolContext<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<ToolEffect, String>> + Send + 'a>> {
        Box::pin(async move {
            let task_id = args["task_id"]
                .as_str()
                .ok_or("Missing required parameter: task_id")?;
            let tasks = ctx.state.tasks.lock().map_err(|e| e.to_string())?;
            let Some(entry) = tasks.iter().find(|t| t.id == task_id) else {
                return Err(format!("Task not found: {task_id}"));
            };
            let tail = crate::task::tail_chars(&entry.output, 16000);
            let tail = if tail.is_empty() {
                "(no output yet)".to_string()
            } else {
                tail
            };
            Ok(ToolEffect::plain(format!(
                "Output of task {} ({}, {}):\n{tail}",
                entry.id,
                task_status_label(entry.status),
                task_duration_label(entry.started_at, entry.ended_at)
            )))
        })
    }
}

pub(crate) struct TaskStop;

impl Tool for TaskStop {
    fn name(&self) -> &'static str {
        "TaskStop"
    }

    /// It kills only background tasks this session started itself — risk equivalent to in-session state cleanup;
    /// also allowed in plan mode (same policy as TodoList write operations)
    fn read_only(&self) -> bool {
        true
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "TaskStop",
                "description": "Stop (kill) a background Bash task that is still running.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "task_id": { "type": "string", "description": "Background task id (b1, b2, ...)" }
                    },
                    "required": ["task_id"]
                }
            }
        })
    }

    fn execute<'a>(
        &'a self,
        args: serde_json::Value,
        ctx: ToolContext<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<ToolEffect, String>> + Send + 'a>> {
        Box::pin(async move {
            let task_id = args["task_id"]
                .as_str()
                .ok_or("Missing required parameter: task_id")?;
            let result = crate::task::stop_task(
                &ctx.state.tasks,
                task_id,
                &ctx.state.task_notify,
                &ctx.state.session_id,
            )?;
            Ok(ToolEffect::plain(result))
        })
    }
}

pub(crate) struct AskUserQuestionTool;

/// ExitPlanMode: the model requests exiting plan mode. The session layer intercepts it before plan mode's hard reject and forces a dialog
/// (same as ZCode); the tool implementation is only a defensive fallback — the normal path never reaches execute.
pub(crate) struct ExitPlanModeTool;
impl Tool for ExitPlanModeTool {
    fn name(&self) -> &'static str {
        "ExitPlanMode"
    }

    /// Read-only marker: the plan-mode hard reject only blocks non-read-only tools; this tool is taken over by the session layer's dedicated dialog
    fn read_only(&self) -> bool {
        true
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "ExitPlanMode",
                "description": "Call when the plan has been written to the plan file and you are ready to execute: the user confirms, then plan mode exits. Only available in plan mode.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "plan": { "type": "string", "description": "Optional. Usually omit it — by default the full text of the plan file `.pigcode/plans/plan-<session_id>.md` is used; providing it overrides the file content" }
                    }
                }
            }
        })
    }

    fn execute<'a>(
        &'a self,
        _args: serde_json::Value,
        _ctx: ToolContext<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<ToolEffect, String>> + Send + 'a>> {
        // Defensive: the normal path is intercepted in the session.rs tool loop; execution never gets here
        Box::pin(async move { Err("ExitPlanMode is handled by the session layer".to_string()) })
    }
}

/// EnterPlanMode: the model voluntarily enters plan mode (research first and produce a plan when the task is complex or the change is large).
/// Entering plan mode is self-restriction (going read-only); the session layer switches directly without a dialog; the tool implementation is only a defensive fallback.
pub(crate) struct EnterPlanModeTool;

impl Tool for EnterPlanModeTool {
    fn name(&self) -> &'static str {
        "EnterPlanMode"
    }

    /// Read-only marker: approval-free and not blocked under plan mode (idempotent hint)
    fn read_only(&self) -> bool {
        true
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "EnterPlanMode",
                "description": "Call when the task is complex or the change is large: enter plan mode (read-only research), then ask the user to confirm the written plan with ExitPlanMode.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "reason": { "type": "string", "description": "Why plan mode is being entered (optional; recorded only)" }
                    }
                }
            }
        })
    }

    fn execute<'a>(
        &'a self,
        _args: serde_json::Value,
        _ctx: ToolContext<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<ToolEffect, String>> + Send + 'a>> {
        // Defensive: the normal path is intercepted in the session.rs tool loop; execution never gets here
        Box::pin(async move { Err("EnterPlanMode is handled by the session layer".to_string()) })
    }
}

/// Agent: delegates a subagent to handle an independent subtask. The session layer intercepts execution in the tool loop
/// (run_subagent, a synchronous foreground loop); the tool implementation only registers the schema and description.
pub struct AgentTool {
    /// Available subagent type list (agent_description_list result), spliced into the description
    profiles_summary: String,
}

impl AgentTool {
    pub fn new(profiles: &[crate::agent::AgentProfile]) -> Self {
        Self {
            profiles_summary: crate::agent::agent_description_list(profiles),
        }
    }
}

impl Tool for AgentTool {
    fn name(&self) -> &'static str {
        "Agent"
    }

    /// Non-read-only: keeps the plan-mode hard-reject semantics sound (subagents may modify files)
    fn read_only(&self) -> bool {
        false
    }

    fn is_shell(&self) -> bool {
        false
    }

    fn schema(&self) -> serde_json::Value {
        let description = format!(
            "Launch a subagent to handle a task. The subagent runs independently with its own context — it cannot see any messages from this session, so the prompt must be self-contained (brief it like a colleague who just walked in: state the goal, what you already know, and exact file paths).\n\
             Benefit: the subagent's intermediate work (bulk file reads/searches) stays out of this context; you only receive its final conclusion.\n\
             - For lookups, give exact paths or commands; for investigations, give the question, not prescribed steps.\n\
             - Do not delegate trivial one-or-two-step tasks; while a subagent is running, do not redo its work in parallel, and do not abandon it midway to finish the job manually.\n\
             - To apply one task to many objects (fan-out of one template x N items), use AgentSwarm to run them concurrently in a single call.\n\
             - A subagent's result is visible only to you (the user cannot see it); relay it yourself when needed.\n\
             - run_in_background=true returns immediately (with a task_id); you will be notified on completion — **the full result is in the file the notification points to; read it with Read** — do not poll. Prefer resuming an existing subagent with resume over starting a fresh instance.\n\
             Available subagent types (default when subagent_type is omitted: general-purpose):\n\
             {}",
            self.profiles_summary
        );
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "Agent",
                "description": description,
                "parameters": {
                    "type": "object",
                    "properties": {
                        "description": { "type": "string", "description": "A 3-5 word task summary, shown in the UI" },
                        "prompt": { "type": "string", "description": "The complete self-contained task brief (the subagent cannot see any messages from this session)" },
                        "subagent_type": { "type": "string", "description": "Subagent type; defaults to general-purpose when omitted; mutually exclusive with resume" },
                        "run_in_background": { "type": "boolean", "description": "true returns immediately (with a task_id) and runs the subagent in the background; you will be notified on completion and the full result is in the file the notification points to (read it with Read)" },
                        "resume": { "type": "string", "description": "An existing agent_id to continue in its context (mutually exclusive with subagent_type)" }
                    },
                    "required": ["description", "prompt"]
                }
            }
        })
    }

    fn execute<'a>(
        &'a self,
        _args: serde_json::Value,
        _ctx: ToolContext<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<ToolEffect, String>> + Send + 'a>> {
        // Defensive: the normal path is intercepted in the session.rs tool loop (run_subagent); execution never gets here
        Box::pin(async move { Err("Agent is handled by the session layer".to_string()) })
    }
}

/// AgentSwarm: bulk parallel subagents (one prompt template × N items). Schema-only;
/// the session layer intercepts it in the tool loop and runs the swarm concurrently (agent/swarm.rs preparation + task.rs
/// global concurrency slots); the tool implementation only registers the schema and description.
pub struct AgentSwarmTool {
    /// Available subagent type list (agent_description_list result), spliced into the description
    profiles_summary: String,
}

impl AgentSwarmTool {
    pub fn new(profiles: &[crate::agent::AgentProfile]) -> Self {
        Self {
            profiles_summary: crate::agent::agent_description_list(profiles),
        }
    }
}

impl Tool for AgentSwarmTool {
    fn name(&self) -> &'static str {
        "AgentSwarm"
    }

    /// Non-read-only: keeps the plan-mode hard-reject semantics sound (subagents may modify files)
    fn read_only(&self) -> bool {
        false
    }

    fn is_shell(&self) -> bool {
        false
    }

    fn schema(&self) -> serde_json::Value {
        let description = format!(
            "Parallel subagents in bulk: one prompt template x N items — the {{{{item}}}} placeholder in the template is replaced by each item and launches one subagent apiece, all running concurrently (a global concurrency cap queues the overflow).\n\
             - Best for applying the same task to many objects (review these files one by one / migrate these modules one by one); items carry only what varies (path/name/argument), with the shared background and requirements spelled out in the template (subagents cannot see this session — prompts must be self-contained).\n\
             - For one or two dissimilar tasks, use Agent instead; expanded prompts must be pairwise distinct; items are capped at {MAX_SWARM_ITEMS}.\n\
             - resume_agent_ids continues existing subagents (existing agent_id -> appended prompt) and can be combined with items; subagent_type is meaningless for resume-only calls.\n\
             - Foreground by default: blocks until all finish and returns aggregated results (failed items carry their error; each item's full result is in the result.md file named in the aggregated result — read it with Read); cancelling during the run cancels all subagents.\n\
             - run_in_background=true returns immediately with per-item receipts (agent_id/task_id/status) and runs the swarm in the background: each item's completion or failure arrives as a <task-notification> (the full result is in the file the notification points to; read it with Read) — do not poll; background subagents are not cancelled with this session's turns and can be stopped individually with TaskStop.\n\
             Available subagent types (default when subagent_type is omitted: general-purpose):\n\
             {}",
            self.profiles_summary
        );
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "AgentSwarm",
                "description": description,
                "parameters": {
                    "type": "object",
                    "properties": {
                        "prompt_template": { "type": "string", "description": "Task template: the {{item}} placeholder is replaced by each item; spell out the background, requirements, and output format here" },
                        "items": {
                            "type": "array",
                            "description": "The batch of objects: one subagent per item (at least 2 when using items alone)",
                            "items": { "type": "string" }
                        },
                        "resume_agent_ids": {
                            "type": "object",
                            "description": "Optional: existing agent_id -> appended prompt (resumes existing subagents; can be combined with items)",
                            "additionalProperties": { "type": "string" }
                        },
                        "subagent_type": { "type": "string", "description": "Subagent type; defaults to general-purpose when omitted; applies only to subagents newly launched from items" },
                        "run_in_background": { "type": "boolean", "description": "true returns immediately (listing agent_id/task_id per item) and runs the swarm in the background; each item's completion/failure arrives as a <task-notification>, with the full result in the file the notification points to (read it with Read) — do not poll" }
                    },
                    "required": ["prompt_template"]
                }
            }
        })
    }

    fn execute<'a>(
        &'a self,
        _args: serde_json::Value,
        _ctx: ToolContext<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<ToolEffect, String>> + Send + 'a>> {
        // Defensive: the normal path is intercepted in the session.rs tool loop (run_swarm); execution never gets here
        Box::pin(async move { Err("AgentSwarm is handled by the session layer".to_string()) })
    }
}

/// AgentSwarm items cap (after expansion, one subagent per item)
pub const MAX_SWARM_ITEMS: usize = 128;

/// One expanded AgentSwarm subtask
#[derive(Debug)]
pub struct SwarmTask {
    /// Resume target (an existing agent_id); None = start fresh
    pub resume: Option<String>,
    /// Brief summary for the subagent card display (first line of the item / appended prompt, truncated)
    pub description: String,
    /// Final prompt: the items path has {{item}} expanded; the resume path is the appended prompt verbatim
    pub prompt: String,
}

/// AgentSwarm parse result (the complete plan before execution)
#[derive(Debug)]
pub struct SwarmPlan {
    pub tasks: Vec<SwarmTask>,
    /// Profile lookup name for freshly launched subagents (default general-purpose; meaningless for resume-only calls, rejected by validation)
    pub subagent_type: String,
}

impl SwarmPlan {
    /// Count of freshly launched subtasks (expanded from items; resume entries not counted)
    pub fn item_count(&self) -> usize {
        self.tasks.iter().filter(|t| t.resume.is_none()).count()
    }
}

/// Parse and validate AgentSwarm arguments (a pure function for unit testing; executed after the session layer intercepts):
/// at least one of items / resume_agent_ids; items-only needs >= 2; items are capped at
/// MAX_SWARM_ITEMS; the template must contain the {{item}} placeholder; expanded prompts must be pairwise distinct.
pub fn parse_swarm_args(args: &serde_json::Value) -> Result<SwarmPlan, String> {
    let template = args["prompt_template"]
        .as_str()
        .ok_or("Missing required parameter: prompt_template (the task template containing the {{item}} placeholder)")?
        .trim()
        .to_string();
    // items: optional string array; blank/non-string elements error out (silently dropping them would make the model misjudge the concurrency)
    let mut items: Vec<String> = Vec::new();
    if !args["items"].is_null() {
        let array = args["items"]
            .as_array()
            .ok_or("Parameter items must be an array of strings")?;
        for (ix, item) in array.iter().enumerate() {
            let item = item
                .as_str()
                .ok_or_else(|| format!("items element {} must be a string", ix + 1))?
                .trim();
            if item.is_empty() {
                return Err(format!("items element {} must not be empty", ix + 1));
            }
            items.push(item.to_string());
        }
    }
    // resume_agent_ids: optional object map (agent_id → appended prompt); explicitly sorted by key below
    // so task order is deterministic (serde_json Map iteration order is affected by the preserve_order feature and must not be relied upon)
    let mut resumes: Vec<(String, String)> = Vec::new();
    if !args["resume_agent_ids"].is_null() {
        let map = args["resume_agent_ids"]
            .as_object()
            .ok_or("Parameter resume_agent_ids must be an object (agent_id -> appended prompt)")?;
        // serde_json Map iteration order is affected by the preserve_order feature (under a uniform workspace build it
        // may be insertion order rather than key order) — sort explicitly to guarantee deterministic task order
        let mut entries: Vec<(&String, &serde_json::Value)> = map.iter().collect();
        entries.sort_by_key(|(agent_id, _)| agent_id.as_str());
        for (agent_id, prompt) in entries {
            let agent_id = agent_id.trim();
            let prompt = prompt
                .as_str()
                .ok_or_else(|| format!("resume_agent_ids[\"{agent_id}\"] must be a string"))?
                .trim();
            if agent_id.is_empty() {
                return Err("resume_agent_ids contains an empty agent_id key".to_string());
            }
            if prompt.is_empty() {
                return Err(format!(
                    "resume_agent_ids[\"{agent_id}\"] has an empty appended prompt"
                ));
            }
            resumes.push((agent_id.to_string(), prompt.to_string()));
        }
    }
    if items.is_empty() && resumes.is_empty() {
        return Err("Provide at least one of items or resume_agent_ids".to_string());
    }
    if resumes.is_empty() && items.len() < 2 {
        return Err(
            "An items-only batch needs at least 2 items (use the Agent tool for a single task)"
                .to_string(),
        );
    }
    if items.len() > MAX_SWARM_ITEMS {
        return Err(format!(
            "items is capped at {MAX_SWARM_ITEMS} (got {})",
            items.len()
        ));
    }
    let subagent_type = args["subagent_type"].as_str().unwrap_or("").trim();
    if items.is_empty() && !subagent_type.is_empty() {
        return Err(
            "subagent_type is meaningless for resume-only calls (resumed subagents re-resolve their own profiles)"
                .to_string(),
        );
    }
    let mut tasks: Vec<SwarmTask> = Vec::new();
    if !items.is_empty() {
        if template.is_empty() {
            return Err("prompt_template must not be empty".to_string());
        }
        if !template.contains("{{item}}") {
            return Err(
                "prompt_template is missing the {{item}} placeholder (each item is substituted into the template)"
                    .to_string(),
            );
        }
        // Pairwise dedup after expansion: a duplicate prompt = a duplicate item (placeholder presence already validated above)
        let mut seen = std::collections::HashSet::new();
        for item in &items {
            let prompt = template.replace("{{item}}", item);
            if !seen.insert(prompt.clone()) {
                return Err(format!(
                    "Duplicate expanded prompt: item \"{}\" expands to the same prompt as another item (check for duplicate items)",
                    item.chars().take(60).collect::<String>()
                ));
            }
            tasks.push(SwarmTask {
                resume: None,
                description: swarm_task_description(item),
                prompt,
            });
        }
    }
    for (agent_id, prompt) in resumes {
        tasks.push(SwarmTask {
            resume: Some(agent_id),
            description: swarm_task_description(&prompt),
            prompt,
        });
    }
    Ok(SwarmPlan {
        tasks,
        subagent_type: if subagent_type.is_empty() {
            "general-purpose".to_string()
        } else {
            subagent_type.to_string()
        },
    })
}

/// Subagent card summary: first line truncated to 60 characters (the item / appended prompt may be very long)
fn swarm_task_description(text: &str) -> String {
    let first_line = text.lines().next().unwrap_or("").trim();
    first_line.chars().take(60).collect()
}

/// Parse and validate AskUserQuestion arguments: 1-4 questions; per question, a non-empty question, 2-4 options,
/// and non-empty labels. A pure function for unit testing; the actual request/wait is intercepted in the session.rs tool loop.
pub fn parse_questions(
    args: &serde_json::Value,
) -> Result<Vec<pig_protocol::QuestionItem>, String> {
    let items = args["questions"]
        .as_array()
        .ok_or("Missing required parameter: questions (array)")?;
    if items.is_empty() || items.len() > 4 {
        return Err(format!(
            "questions must contain 1-4 items (got {})",
            items.len()
        ));
    }
    let mut questions = Vec::new();
    for (ix, item) in items.iter().enumerate() {
        let n = ix + 1;
        let question = item["question"].as_str().unwrap_or("").trim().to_string();
        if question.is_empty() {
            return Err(format!("Question {n}: question must not be empty"));
        }
        let header = item["header"]
            .as_str()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let multi_select = item["multi_select"].as_bool().unwrap_or(false);
        let options = item["options"]
            .as_array()
            .ok_or_else(|| format!("Question {n} is missing options (array)"))?;
        if options.len() < 2 || options.len() > 4 {
            return Err(format!(
                "Question {n}: options must contain 2-4 items (got {})",
                options.len()
            ));
        }
        let mut parsed_options = Vec::new();
        for option in options {
            let label = option["label"].as_str().unwrap_or("").trim().to_string();
            if label.is_empty() {
                return Err(format!("Question {n} has an option with an empty label"));
            }
            let description = option["description"].as_str().map(str::to_string);
            parsed_options.push(pig_protocol::QuestionOption { label, description });
        }
        questions.push(pig_protocol::QuestionItem {
            question,
            header,
            multi_select,
            options: parsed_options,
        });
    }
    Ok(questions)
}

impl Tool for AskUserQuestionTool {
    fn name(&self) -> &'static str {
        "AskUserQuestion"
    }

    fn read_only(&self) -> bool {
        true
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "AskUserQuestion",
                "description": "When a user decision is needed, ask 1-4 structured questions (2-4 options each) for the user to pick from, instead of asking in plain text. Each question may set multi_select to allow multiple choices; the UI automatically appends an \"Other\" free-text option.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "questions": {
                            "type": "array",
                            "description": "1-4 questions",
                            "minItems": 1,
                            "maxItems": 4,
                            "items": {
                                "type": "object",
                                "properties": {
                                    "question": { "type": "string", "description": "The full question text" },
                                    "header": { "type": "string", "description": "Optional short label (<=12 chars)" },
                                    "multi_select": { "type": "boolean", "description": "Optional; true allows multiple selections (default false)" },
                                    "options": {
                                        "type": "array",
                                        "description": "2-4 options",
                                        "minItems": 2,
                                        "maxItems": 4,
                                        "items": {
                                            "type": "object",
                                            "properties": {
                                                "label": { "type": "string", "description": "Option label" },
                                                "description": { "type": "string", "description": "Optional supplementary description" }
                                            },
                                            "required": ["label"]
                                        }
                                    }
                                },
                                "required": ["question", "options"]
                            }
                        }
                    },
                    "required": ["questions"]
                }
            }
        })
    }

    fn execute<'a>(
        &'a self,
        _args: serde_json::Value,
        _ctx: ToolContext<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<ToolEffect, String>> + Send + 'a>> {
        // Defensive: the normal path is intercepted in the session.rs tool loop; execution never gets here
        Box::pin(async move { Err("AskUserQuestion is handled by the session layer".to_string()) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------- parse_swarm_args ----------

    #[test]
    fn swarm_expand_items() {
        let plan = parse_swarm_args(&serde_json::json!({
            "prompt_template": "Review {{item}} and output the issue list",
            "items": ["src/a.rs", "src/b.rs"]
        }))
        .expect("valid call");
        assert_eq!(plan.tasks.len(), 2);
        assert_eq!(
            plan.tasks[0].prompt,
            "Review src/a.rs and output the issue list"
        );
        assert_eq!(
            plan.tasks[1].prompt,
            "Review src/b.rs and output the issue list"
        );
        assert!(
            plan.tasks.iter().all(|t| t.resume.is_none()),
            "items tasks carry no resume"
        );
        assert_eq!(plan.subagent_type, "general-purpose", "default profile");
        assert_eq!(plan.item_count(), 2);
        // All placeholders are replaced
        let plan = parse_swarm_args(&serde_json::json!({
            "prompt_template": "Read {{item}} and compare the callers of {{item}}",
            "items": ["a", "b"]
        }))
        .expect("multiple placeholders");
        assert_eq!(plan.tasks[0].prompt, "Read a and compare the callers of a");
    }

    #[test]
    fn swarm_missing_placeholder_err() {
        let err = parse_swarm_args(&serde_json::json!({
            "prompt_template": "review these files",
            "items": ["a", "b"]
        }))
        .unwrap_err();
        assert!(
            err.contains("{{item}}"),
            "the error should name the missing placeholder: {err}"
        );
    }

    #[test]
    fn swarm_duplicate_expansion_err() {
        let err = parse_swarm_args(&serde_json::json!({
            "prompt_template": "Review {{item}}",
            "items": ["a.rs", "a.rs"]
        }))
        .unwrap_err();
        assert!(
            err.contains("Duplicate"),
            "duplicate items should be rejected: {err}"
        );
        // Items that expand identically after trimming surrounding whitespace also count as duplicates
        let err = parse_swarm_args(&serde_json::json!({
            "prompt_template": "Review {{item}}",
            "items": ["a.rs", "  a.rs  "]
        }))
        .unwrap_err();
        assert!(
            err.contains("Duplicate"),
            "duplicates after trim should also be rejected: {err}"
        );
    }

    #[test]
    fn swarm_items_limit() {
        let items: Vec<String> = (0..MAX_SWARM_ITEMS).map(|i| format!("f{i}")).collect();
        let plan = parse_swarm_args(&serde_json::json!({
            "prompt_template": "Process {{item}}",
            "items": items
        }))
        .expect("exactly at the cap should pass");
        assert_eq!(plan.tasks.len(), MAX_SWARM_ITEMS);
        let items: Vec<String> = (0..=MAX_SWARM_ITEMS).map(|i| format!("f{i}")).collect();
        let err = parse_swarm_args(&serde_json::json!({
            "prompt_template": "Process {{item}}",
            "items": items
        }))
        .unwrap_err();
        assert!(err.contains("capped"), "over the cap should error: {err}");
    }

    #[test]
    fn swarm_shape_rules() {
        // Neither category provided
        let err =
            parse_swarm_args(&serde_json::json!({"prompt_template": "x {{item}}"})).unwrap_err();
        assert!(err.contains("at least one of"), "{err}");
        // items-only with a single element
        let err = parse_swarm_args(&serde_json::json!({
            "prompt_template": "x {{item}}",
            "items": ["only"]
        }))
        .unwrap_err();
        assert!(err.contains("at least 2"), "{err}");
        // blank item / non-string item
        assert!(
            parse_swarm_args(&serde_json::json!({
                "prompt_template": "x {{item}}",
                "items": ["a", "  "]
            }))
            .is_err(),
            "blank items should be rejected"
        );
        assert!(
            parse_swarm_args(&serde_json::json!({
                "prompt_template": "x {{item}}",
                "items": ["a", 1]
            }))
            .is_err(),
            "non-string items should be rejected"
        );
        // empty template + items
        assert!(
            parse_swarm_args(&serde_json::json!({
                "prompt_template": "  ",
                "items": ["a", "b"]
            }))
            .is_err(),
            "an empty template should be rejected"
        );
    }

    #[test]
    fn swarm_resume_rules() {
        // resume-only is valid (prompt_template is schema-required but ignored here)
        let plan = parse_swarm_args(&serde_json::json!({
            "prompt_template": "",
            "resume_agent_ids": {"a2": "check further", "a1": "continue"}
        }))
        .expect("resume-only call");
        assert_eq!(plan.tasks.len(), 2);
        assert_eq!(
            plan.tasks[0].resume.as_deref(),
            Some("a1"),
            "the object map iterates in key order"
        );
        assert_eq!(plan.item_count(), 0);
        // resume-only + subagent_type → error
        assert!(
            parse_swarm_args(&serde_json::json!({
                "prompt_template": "",
                "resume_agent_ids": {"a1": "continue"},
                "subagent_type": "explore"
            }))
            .is_err(),
            "subagent_type on a resume-only call should be rejected"
        );
        // items + resume mixed: a single item is valid; items come first
        let plan = parse_swarm_args(&serde_json::json!({
            "prompt_template": "Process {{item}}",
            "items": ["one"],
            "resume_agent_ids": {"a1": "continue"},
            "subagent_type": "explore"
        }))
        .expect("mixed with a single item");
        assert_eq!(plan.tasks.len(), 2);
        assert!(plan.tasks[0].resume.is_none(), "items tasks come first");
        assert_eq!(plan.tasks[1].resume.as_deref(), Some("a1"));
        assert_eq!(plan.subagent_type, "explore");
        // empty appended prompt
        assert!(
            parse_swarm_args(&serde_json::json!({
                "prompt_template": "",
                "resume_agent_ids": {"a1": "  "}
            }))
            .is_err(),
            "an empty appended prompt should be rejected"
        );
    }

    #[test]
    fn swarm_description_first_line_truncated() {
        let long = format!("{}\nsecond line unused", "x".repeat(100));
        let plan = parse_swarm_args(&serde_json::json!({
            "prompt_template": "Process {{item}}",
            "items": [long, "short"]
        }))
        .expect("long item");
        assert_eq!(
            plan.tasks[0].description.chars().count(),
            60,
            "the description takes the first line truncated to 60 chars"
        );
        assert_eq!(plan.tasks[1].description, "short");
    }

    #[test]
    fn swarm_run_in_background_not_in_plan() {
        // run_in_background is an execution-mode switch, not part of SwarmPlan (validation rules unaffected)
        let with = parse_swarm_args(&serde_json::json!({
            "prompt_template": "Review {{item}}",
            "items": ["a.rs", "b.rs"],
            "run_in_background": true
        }))
        .expect("valid call with the background switch");
        let without = parse_swarm_args(&serde_json::json!({
            "prompt_template": "Review {{item}}",
            "items": ["a.rs", "b.rs"]
        }))
        .expect("valid call without the background switch");
        assert_eq!(with.tasks.len(), without.tasks.len());
        for (a, b) in with.tasks.iter().zip(without.tasks.iter()) {
            assert_eq!(a.prompt, b.prompt);
            assert_eq!(a.description, b.description);
            assert_eq!(a.resume, b.resume);
        }
        assert_eq!(with.subagent_type, without.subagent_type);
    }
}
