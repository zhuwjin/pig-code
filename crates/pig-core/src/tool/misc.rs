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
    status.as_str().to_string()
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
                "description": "List the background tasks of this session (id, status, command, elapsed time).",
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
                "description": "Read a background task's output (tail excerpt).",
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
            let summary = entry.summary();
            let mut output = summary
                .spill_path
                .as_ref()
                .and_then(|path| std::fs::read(path).ok())
                .and_then(|bytes| {
                    pig_utils::text::decode(&bytes)
                        .ok()
                        .map(|document| document.text)
                })
                .unwrap_or_else(|| entry.output.clone());
            if let Some(reason) = summary.reason.as_deref()
                && !output.contains(reason)
            {
                output.push_str("\n\n");
                output.push_str(reason);
            }
            let tail = crate::task::tail_chars(&output, 16000);
            let tail = if tail.is_empty() {
                "(no output yet)".to_string()
            } else {
                tail
            };
            Ok(ToolEffect::plain(format!(
                "Output of task {} ({}, {}):\n{tail}",
                summary.id,
                task_status_label(summary.status),
                task_duration_label(
                    summary.started_at_ms / 1_000,
                    summary.ended_at_ms.map(|value| value / 1_000)
                )
            )))
        })
    }
}

struct TaskWaitBatch {
    notifications: Vec<pig_protocol::TaskNotification>,
    delivered_notification_ids: Vec<String>,
    active: Vec<pig_protocol::TaskSummary>,
}

fn task_wait_batch(
    state: &crate::task::SessionToolState,
    task_ids: &std::collections::HashSet<String>,
) -> Result<TaskWaitBatch, String> {
    let tasks = state.tasks.lock().map_err(|error| error.to_string())?;
    for id in task_ids {
        if !tasks.iter().any(|entry| &entry.id == id) {
            return Err(format!("Task not found: {id}"));
        }
    }
    let candidates = tasks
        .iter()
        .filter(|entry| {
            !entry.foreground
                && entry.status.is_terminal()
                && (task_ids.is_empty() || task_ids.contains(&entry.id))
        })
        .map(|entry| {
            (
                crate::task::notification::terminal_notification(&state.session_id, entry),
                entry.terminal_notification_suppressed,
            )
        })
        .collect::<Vec<_>>();
    let active = tasks
        .iter()
        .filter(|entry| {
            !entry.foreground
                && matches!(
                    entry.status,
                    pig_protocol::TaskStatus::Queued | pig_protocol::TaskStatus::Running
                )
                && (task_ids.is_empty() || task_ids.contains(&entry.id))
        })
        .map(|entry| entry.summary())
        .collect::<Vec<_>>();
    drop(tasks);

    let receipts = state
        .delivery_receipts
        .lock()
        .map_err(|error| error.to_string())?;
    let store = state
        .task_store
        .as_ref()
        .map(|store| store.lock().expect("store lock"));
    let mut notifications = Vec::new();
    let mut delivered_notification_ids = Vec::new();
    for (notification, suppressed) in candidates {
        let mut deliverable = false;
        if let Some(store) = store.as_ref()
            && !suppressed
        {
            store
                .schedule_notification(&state.session_id, &notification)
                .map_err(|error| format!("{error:?}"))?;
            let delivered = matches!(
                store
                    .notification_delivery(&notification.id)
                    .map_err(|error| format!("{error:?}"))?,
                Some((pig_protocol::DeliveryState::Delivered, _))
            );
            deliverable = !delivered && !receipts.contains_key(&notification.id);
        }
        if !task_ids.is_empty() || deliverable {
            notifications.push(notification.clone());
        }
        if deliverable {
            delivered_notification_ids.push(notification.id);
        }
    }
    Ok(TaskWaitBatch {
        notifications,
        delivered_notification_ids,
        active,
    })
}

pub(crate) struct TaskWait;

impl Tool for TaskWait {
    fn name(&self) -> &'static str {
        "TaskWait"
    }

    fn read_only(&self) -> bool {
        true
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "TaskWait",
                "description": "Wait for background tasks to reach a terminal state. Root-session results acknowledge returned notification IDs after the tool result is durable; subagent waits are queries and do not consume the parent's notifications.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "task_ids": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Optional task ids to wait for; omit to wait for any undelivered terminal update in this session"
                        },
                        "timeout_ms": { "type": "integer", "description": "Maximum wait time in milliseconds (default 30000, max 300000)" }
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
            let task_ids = match args.get("task_ids") {
                None | Some(serde_json::Value::Null) => Default::default(),
                Some(serde_json::Value::Array(values)) => values
                    .iter()
                    .map(|value| {
                        value
                            .as_str()
                            .filter(|id| !id.is_empty())
                            .map(str::to_string)
                            .ok_or_else(|| "task_ids must contain non-empty strings".to_string())
                    })
                    .collect::<Result<std::collections::HashSet<_>, _>>()?,
                Some(_) => return Err("task_ids must be an array of task IDs".to_string()),
            };
            let timeout_ms = match args.get("timeout_ms") {
                None => 30_000,
                Some(value) => value
                    .as_u64()
                    .ok_or("timeout_ms must be a non-negative integer")?,
            }
            .clamp(1, 300_000);
            let deadline =
                tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
            let wait_owner = ctx
                .state
                .owner_agent_id
                .clone()
                .unwrap_or_else(|| ctx.state.session_id.clone());
            let mut wait_interest =
                crate::task::TaskWaitInterest::register(ctx.state, &wait_owner, &task_ids);
            let mut revision = ctx.state.tasks.subscribe();
            loop {
                let batch = task_wait_batch(ctx.state, &task_ids)?;
                if !batch.notifications.is_empty() {
                    wait_interest.delivered(&batch.notifications);
                    let mut output =
                        crate::task::notification::render_notification_batch(&batch.notifications);
                    if ctx.state.owner_agent_id.is_none()
                        && batch.delivered_notification_ids.is_empty()
                    {
                        output = format!(
                            "No new notification delivery is acknowledged by this result.\n\n{output}"
                        );
                    }
                    let mut effect = ToolEffect::plain(output);
                    effect.delivered_notification_ids = batch.delivered_notification_ids;
                    return Ok(effect);
                }
                if batch.active.is_empty() {
                    return Ok(ToolEffect::plain(
                        "No running tasks or undelivered terminal notifications.".to_string(),
                    ));
                }
                if tokio::time::Instant::now() >= deadline {
                    let active = batch
                        .active
                        .iter()
                        .map(|task| format!("{} [{}]", task.id, task.status.as_str()))
                        .collect::<Vec<_>>()
                        .join(", ");
                    return Ok(ToolEffect::plain(format!(
                        "Wait timed out with no terminal task updates. Active tasks: {active}"
                    )));
                }
                tokio::select! {
                    _ = revision.changed() => {}
                    _ = tokio::time::sleep_until(deadline) => {
                        return Ok(ToolEffect::plain(
                            "Wait timed out with no terminal task updates.".to_string(),
                        ));
                    }
                }
            }
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
                "description": "Stop (kill) a background task that is still running.",
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
            let result = crate::task::stop_task_with_agents(
                &ctx.state.tasks,
                task_id,
                &ctx.state.task_notify,
                &ctx.state.session_id,
                Some(&ctx.state.agents),
            )
            .await?;
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
        Self::from_summary(&crate::agent::agent_description_list(profiles))
    }

    pub fn from_summary(summary: &str) -> Self {
        Self {
            profiles_summary: summary.to_string(),
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
            "Launch a subagent to handle a task. The subagent runs independently with its own context — by default it cannot see any messages from this session, so the prompt must be self-contained (brief it like a colleague who just walked in: state the goal, what you already know, and exact file paths).\n\
             Benefit: the subagent's intermediate work (bulk file reads/searches) stays out of this context; you only receive its final conclusion.\n\
             - For lookups, give exact paths or commands; for investigations, give the question, not prescribed steps.\n\
             - Do not delegate trivial one-or-two-step tasks; while a subagent is running, do not redo its work in parallel, and do not abandon it midway to finish the job manually.\n\
             - To apply one task to many objects (fan-out of one template x N items), use AgentSwarm to run them concurrently in a single call.\n\
             - A subagent's result is visible only to you (the user cannot see it); relay it yourself when needed.\n\
             - run_in_background=true returns immediately (with a task_id); you will be notified on completion — **the full result is in the file the notification points to; read it with Read** — do not poll. Prefer resuming an existing subagent with resume over starting a fresh instance.\n\
             - fork=true creates a new child with a snapshot of this session's current effective history; fork and resume are mutually exclusive.\n\
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
                        "resume": { "type": "string", "description": "An existing agent_id to continue in its context (mutually exclusive with subagent_type)" },
                        "fork": { "type": "boolean", "description": "true creates a new child seeded with the parent's current effective-history snapshot; mutually exclusive with resume" },
                        "output_schema": { "type": "object", "description": "Optional object-rooted JSON Schema for the child's final structured result" },
                        "cwd": { "type": "string", "description": "Optional existing child working directory; mutually exclusive with worktree and resume" },
                        "worktree": { "type": "boolean", "description": "Create an isolated Git worktree from HEAD for a fresh or fork child; uncommitted changes are not copied" }
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

/// SendMessage: deliver a message to a RUNNING subagent mid-flight
/// (coordinator steering, ZCode SendMessage parity). The drive loop drains
/// the shared registry at its step boundaries and appends the message to
/// its conversation as a user turn; practically this reaches background
/// subagents (a foreground Agent call blocks this session's tool loop until
/// it returns).
pub struct SendMessageTool;

impl Tool for SendMessageTool {
    fn name(&self) -> &'static str {
        "SendMessage"
    }

    fn read_only(&self) -> bool {
        true
    }

    fn is_shell(&self) -> bool {
        false
    }

    fn schema(&self) -> serde_json::Value {
        let description = "Send a message to a running subagent mid-flight. The message is appended to its conversation at its next step boundary — use it to steer, correct, or add context without waiting for completion. Only currently-running subagents can receive messages (background ones launched with run_in_background=true are the practical targets; a foreground Agent call blocks this session until it returns). For a finished agent, resume it with Agent(resume=...) instead.";
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "SendMessage",
                "description": description,
                "parameters": {
                    "type": "object",
                    "properties": {
                        "agent_id": {
                            "type": "string",
                            "description": "Target subagent's agent_id (from the Agent tool's receipt or a task notification)"
                        },
                        "message": {
                            "type": "string",
                            "description": "Message text, delivered as a user turn in the subagent's conversation"
                        }
                    },
                    "required": ["agent_id", "message"]
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
            let agent_id = args["agent_id"].as_str().ok_or("agent_id is required")?;
            let message = args["message"].as_str().ok_or("message is required")?;
            let result = ctx.state.agents.push_steer(
                agent_id,
                crate::agent::AgentSteer::message(message.to_string()),
            );
            match result {
                Ok(()) => Ok(ToolEffect::plain(format!(
                    "Delivered to {agent_id}: the subagent sees the message at its next step boundary."
                ))),
                Err(error) if ctx.state.agents.is_active(agent_id) => Err(error),
                Err(_) => Err(format!(
                    "No running subagent with agent_id \"{agent_id}\" (it may have finished; resume it with Agent(resume=...))"
                )),
            }
        })
    }
}

/// AgentSwarm: bulk parallel subagents (one prompt template × N items). Schema-only;
/// the session layer intercepts it in the tool loop and runs the swarm concurrently (agent/swarm.rs preparation + task.rs
/// session-tree concurrency slots); the tool implementation only registers the schema and description.
pub struct AgentSwarmTool {
    /// Available subagent type list (agent_description_list result), spliced into the description
    profiles_summary: String,
}

impl AgentSwarmTool {
    pub fn new(profiles: &[crate::agent::AgentProfile]) -> Self {
        Self::from_summary(&crate::agent::agent_description_list(profiles))
    }

    pub fn from_summary(summary: &str) -> Self {
        Self {
            profiles_summary: summary.to_string(),
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
            "Parallel subagents in bulk: one prompt template x N items — the {{{{item}}}} placeholder in the template is replaced by each item and launches one subagent apiece, all running concurrently (a session-tree concurrency cap queues the overflow).\n\
             - Best for applying the same task to many objects (review these files one by one / migrate these modules one by one); items carry only what varies (path/name/argument), with the shared background and requirements spelled out in the template (subagents cannot see this session — prompts must be self-contained).\n\
             - For one or two dissimilar tasks, use Agent instead; expanded prompts must be pairwise distinct; items are capped at {MAX_SWARM_ITEMS}.\n\
             - resume_agent_ids continues existing subagents (existing agent_id -> appended prompt) and can be combined with items; subagent_type is meaningless for resume-only calls.\n\
             - Foreground by default: blocks until all finish and returns aggregated results (failed items carry their error; each item's full result is in the result.md file named in the aggregated result — read it with Read); cancelling during the run cancels all subagents.\n\
             - run_in_background=true returns immediately with per-item receipts (agent_id/task_id/status) and runs the swarm in the background: each terminal notification is delivered automatically; TaskWait waits and TaskOutput reads progress (the full result is in the file the notification points to; read it with Read). Background subagents are not cancelled with this session's turns and can be stopped individually with TaskStop.\n\
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
                        "run_in_background": { "type": "boolean", "description": "true returns immediately (listing agent_id/task_id per item) and runs the swarm in the background; terminal notifications arrive automatically, TaskWait waits, and the full result is in the file the notification points to (read it with Read)" },
                        "output_schema": { "type": "object", "description": "Optional object-rooted JSON Schema applied to every child's final structured result" },
                        "cwd": { "type": "string", "description": "Optional existing working directory shared by newly launched items" },
                        "worktree": { "type": "boolean", "description": "Create a separate Git worktree from HEAD for each newly launched item" }
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

pub(crate) fn parse_child_workspace(
    args: &serde_json::Value,
) -> Result<(Option<std::path::PathBuf>, bool), String> {
    let cwd = match args.get("cwd") {
        None | Some(serde_json::Value::Null) => None,
        Some(value) => {
            let path = value
                .as_str()
                .ok_or("cwd must be a directory path string")?
                .trim();
            if path.is_empty() {
                return Err("cwd must not be empty".into());
            }
            Some(std::path::PathBuf::from(path))
        }
    };
    let worktree = args["worktree"].as_bool().unwrap_or(false);
    if cwd.is_some() && worktree {
        return Err("cwd and worktree are mutually exclusive".into());
    }
    Ok((cwd, worktree))
}

pub(crate) fn parse_agent_output_schema(
    args: &serde_json::Value,
) -> Result<Option<serde_json::Value>, String> {
    let Some(schema) = args.get("output_schema") else {
        return Ok(None);
    };
    if schema.is_null() {
        return Ok(None);
    }
    if !schema.is_object() || schema.get("type").and_then(|kind| kind.as_str()) != Some("object") {
        return Err(
            "output_schema must be an object-rooted JSON Schema with type=\"object\"".into(),
        );
    }
    Ok(Some(schema.clone()))
}

/// AgentSwarm parse result (the complete plan before execution)
#[derive(Debug)]
pub struct SwarmPlan {
    pub tasks: Vec<SwarmTask>,
    pub output_schema: Option<serde_json::Value>,
    pub cwd: Option<std::path::PathBuf>,
    pub worktree: bool,
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
    let output_schema = parse_agent_output_schema(args)?;
    let (cwd, worktree) = parse_child_workspace(args)?;
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
    if items.is_empty() && (cwd.is_some() || worktree) {
        return Err("cwd/worktree apply only to newly launched swarm items".into());
    }
    Ok(SwarmPlan {
        tasks,
        output_schema,
        cwd,
        worktree,
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

#[cfg(test)]
mod task_wait_tests {
    use super::{TaskOutput, TaskWait, task_wait_batch};
    use crate::task::notification::DeliveryReceipt;
    use crate::tool::{ChangeTracker, Tool, ToolContext};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    fn fixture(
        name: &str,
    ) -> (
        crate::task::SessionToolState,
        Arc<Mutex<crate::store::Store>>,
        PathBuf,
    ) {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "pig-task-wait-{name}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("fixture");
        let store = Arc::new(Mutex::new(crate::store::Store::open(&root).expect("store")));
        let state = crate::task::SessionToolState::for_test()
            .with_notification_store(store.clone(), Default::default());
        (state, store, root)
    }

    fn register(state: &crate::task::SessionToolState, queued: bool) -> String {
        let cancel = tokio_util::sync::CancellationToken::new();
        if queued {
            crate::task::register_agent_task_queued(
                state,
                "wait for me".into(),
                cancel,
                "a-wait".into(),
            )
        } else {
            crate::task::register_agent_task(state, "wait for me".into(), cancel, "a-wait".into())
        }
    }

    fn finish(state: &crate::task::SessionToolState, task_id: &str) {
        let mut tasks = state.tasks.lock().expect("tasks");
        let entry = tasks
            .iter_mut()
            .find(|entry| entry.id == task_id)
            .expect("task");
        entry.status = pig_protocol::TaskStatus::Completed;
        entry.exit_code = Some(0);
        entry.ended_at = Some(crate::rollout::now_secs());
        drop(tasks);
        crate::task::note_output(&state.tasks, task_id, "finished\n");
    }

    fn context<'a>(
        state: &'a crate::task::SessionToolState,
        tracker: &'a mut ChangeTracker,
        cwd: &'a Path,
    ) -> ToolContext<'a> {
        ToolContext {
            cwd,
            tracker,
            state,
            fs_grant: None,
            sandbox_override: Default::default(),
            sandbox_profile: Default::default(),
            additional_permissions: Default::default(),
        }
    }

    #[tokio::test]
    async fn wait_wakes_on_terminal_revision_and_deduplicates_delivery() {
        let (mut state, store, root) = fixture("wake");
        let task_id = register(&state, false);
        let initial = task_wait_batch(&state, &Default::default()).expect("initial state");
        assert_eq!(initial.active.len(), 1);
        assert!(initial.notifications.is_empty());

        let waiting = tokio::spawn({
            let state = state.clone();
            let root = root.clone();
            let task_id = task_id.clone();
            async move {
                let mut tracker = ChangeTracker::default();
                TaskWait
                    .execute(
                        serde_json::json!({"task_ids": [task_id], "timeout_ms": 2_000}),
                        context(&state, &mut tracker, &root),
                    )
                    .await
                    .expect("wait")
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        finish(&state, &task_id);
        let effect = waiting.await.expect("join");
        let notification_id = "task:test:b1:completed".to_string();
        assert_eq!(
            effect.delivered_notification_ids,
            std::slice::from_ref(&notification_id)
        );
        assert!(effect.output.contains(&notification_id));
        assert_eq!(
            store
                .lock()
                .expect("store")
                .pending_notifications("test")
                .expect("pending")
                .len(),
            1
        );

        state.delivery_receipts.lock().expect("receipts").insert(
            notification_id.clone(),
            DeliveryReceipt {
                channel: pig_protocol::DeliveryChannel::TaskWait,
                confirmed: false,
            },
        );
        let selected = std::collections::HashSet::from([task_id.clone()]);
        let duplicate = task_wait_batch(&state, &selected).expect("query");
        assert_eq!(duplicate.notifications.len(), 1);
        assert!(duplicate.delivered_notification_ids.is_empty());

        let mut tracker = ChangeTracker::default();
        let queried = TaskOutput
            .execute(
                serde_json::json!({"task_id": task_id}),
                context(&state, &mut tracker, &root),
            )
            .await
            .expect("query output");
        assert!(queried.delivered_notification_ids.is_empty());
        assert!(queried.output.contains("finished"));
        assert_eq!(
            store
                .lock()
                .expect("store")
                .pending_notifications("test")
                .expect("pending")
                .len(),
            1,
            "TaskOutput must not consume the pending notification"
        );
        store
            .lock()
            .expect("store")
            .mark_delivered(
                &[notification_id],
                pig_protocol::DeliveryChannel::TaskWait,
                1,
            )
            .expect("confirm");

        state.task_store = None;
        let mut tracker = ChangeTracker::default();
        let child = TaskWait
            .execute(
                serde_json::json!({"task_ids": [task_id], "timeout_ms": 1}),
                context(&state, &mut tracker, &root),
            )
            .await
            .expect("child query");
        assert!(child.delivered_notification_ids.is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn child_wait_marks_the_notification_as_delivered() {
        let (mut state, _store, root) = fixture("child-wait-delivered");
        state.task_store = None;
        state.owner_agent_id = Some("a1".into());
        let task_id = register(&state, false);
        let waiting = tokio::spawn({
            let state = state.clone();
            let root = root.clone();
            let task_id = task_id.clone();
            async move {
                let mut tracker = ChangeTracker::default();
                TaskWait
                    .execute(
                        serde_json::json!({"task_ids": [task_id], "timeout_ms": 2_000}),
                        context(&state, &mut tracker, &root),
                    )
                    .await
                    .expect("wait")
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        finish(&state, &task_id);
        let effect = waiting.await.expect("join");
        assert!(effect.output.contains("task:test:b1:completed"));
        assert!(
            crate::task::task_wait_pending(&state, "a1", &task_id),
            "a successful child wait suppresses the duplicate steer"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn child_wait_removes_an_already_queued_duplicate_steer() {
        let (mut state, _store, root) = fixture("child-wait-queued");
        state.task_store = None;
        state.owner_agent_id = Some("a1".into());
        let task_id = register(&state, false);
        finish(&state, &task_id);
        let queued = "<notification id=\"task:test:b1:completed\">done</notification>".to_string();
        let notification = {
            let tasks = state.tasks.lock().expect("task registry");
            let entry = tasks
                .iter()
                .find(|entry| entry.id == task_id)
                .expect("finished task");
            crate::task::notification::terminal_notification(&state.session_id, entry)
        };
        assert_eq!(notification.id, "task:test:b1:completed");
        state
            .agents
            .activate_child(crate::agent::AgentNode::child(
                "a1".into(),
                "test".into(),
                1,
                "run-a1".into(),
            ))
            .expect("activate child");
        assert!(
            state
                .agents
                .push_steer(
                    "a1",
                    crate::agent::AgentSteer::notification(notification, queued),
                )
                .is_ok()
        );
        let mut tracker = ChangeTracker::default();
        let effect = TaskWait
            .execute(
                serde_json::json!({"task_ids": [task_id], "timeout_ms": 1_000}),
                context(&state, &mut tracker, &root),
            )
            .await
            .expect("wait");
        assert!(effect.output.contains("task:test:b1:completed"));
        assert!(
            state.agents.drain_mailbox("a1").is_empty(),
            "the queued duplicate steer is removed"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn child_wait_timeout_releases_the_suppression() {
        let (mut state, _store, root) = fixture("child-wait-timeout");
        state.task_store = None;
        state.owner_agent_id = Some("a1".into());
        let task_id = register(&state, false);
        let mut tracker = ChangeTracker::default();
        let effect = TaskWait
            .execute(
                serde_json::json!({"task_ids": [task_id.clone()], "timeout_ms": 5}),
                context(&state, &mut tracker, &root),
            )
            .await
            .expect("timeout");
        assert!(effect.output.contains("Wait timed out"));
        assert!(
            !crate::task::task_wait_pending(&state, "a1", &task_id),
            "a timed-out wait must leave the steer path open"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn dropping_wait_requeues_terminal_notification() {
        let (mut state, _store, root) = fixture("wait-drop-requeue");
        let (notify, mut changes) = tokio::sync::mpsc::unbounded_channel();
        state.task_notify = notify;
        let task_id = register(&state, true);
        {
            let mut tasks = state.tasks.lock().expect("tasks");
            let task = tasks
                .iter_mut()
                .find(|task| task.id == task_id)
                .expect("task");
            task.status = pig_protocol::TaskStatus::Completed;
            task.ended_at = Some(task.started_at + 1);
        }
        while changes.try_recv().is_ok() {}
        let wait = crate::task::TaskWaitInterest::register(
            &state,
            "a1",
            &std::collections::HashSet::from([task_id.clone()]),
        );
        drop(wait);
        match changes
            .recv()
            .await
            .expect("terminal notification requeued")
        {
            crate::task::TaskChange::Terminal { task, .. } => assert_eq!(task.summary.id, task_id),
            other => panic!("unexpected task change: {other:?}"),
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn wait_timeout_is_not_a_task_terminal_state() {
        let (state, _store, root) = fixture("timeout");
        let task_id = register(&state, false);
        let mut tracker = ChangeTracker::default();
        let effect = TaskWait
            .execute(
                serde_json::json!({"task_ids": [task_id.clone()], "timeout_ms": 5}),
                context(&state, &mut tracker, &root),
            )
            .await
            .expect("timeout");
        assert!(effect.output.contains("Wait timed out"));
        assert!(effect.delivered_notification_ids.is_empty());
        assert_eq!(
            state.tasks.lock().expect("tasks")[0].status,
            pig_protocol::TaskStatus::Running
        );
        assert!(
            task_wait_batch(&state, &std::collections::HashSet::from(["missing".into()])).is_err()
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn queued_stop_cancels_and_produces_killed() {
        let (state, _store, root) = fixture("stop");
        let task_id = register(&state, true);
        let cancel = state.tasks.lock().expect("tasks")[0]
            .cancel
            .clone()
            .expect("cancel");
        crate::task::stop_task(
            &state.tasks,
            &task_id,
            &state.task_notify,
            &state.session_id,
        )
        .await
        .expect("stop queued");
        assert!(cancel.is_cancelled());
        assert_eq!(
            state.tasks.lock().expect("tasks")[0].status,
            pig_protocol::TaskStatus::Killed
        );
        let notification = crate::task::notification::terminal_notification(
            &state.session_id,
            &state.tasks.lock().expect("tasks")[0],
        );
        assert_eq!(notification.status, pig_protocol::TaskStatus::Killed);
        let _ = std::fs::remove_dir_all(root);
    }
}
