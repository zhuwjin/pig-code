use super::*;

pub(crate) struct TodoListTool;

impl TodoListTool {
    fn render(todos: &[TodoItem]) -> String {
        if todos.is_empty() {
            return "当前没有待办事项".to_string();
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
                "description": "管理会话级待办清单。多步任务开始时拆分为清单写入，执行中随时更新进度；省略 todos 参数读取当前清单，提供则整体替换（非增量）。同一时间至多一项 in_progress。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "todos": {
                            "type": "array",
                            "description": "完整的新待办清单（整体替换）",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "content": { "type": "string", "description": "待办内容" },
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
                            format!("todos 格式非法: {e}（status 须为 pending/in_progress/done）")
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
        pig_protocol::TaskStatus::Running => "运行中".to_string(),
        pig_protocol::TaskStatus::Exited(code) => format!("已退出({code})"),
        pig_protocol::TaskStatus::Killed => "已停止".to_string(),
    }
}

fn task_duration_label(started_at: u64, ended_at: Option<u64>) -> String {
    let secs = ended_at
        .unwrap_or_else(crate::rollout::now_secs)
        .saturating_sub(started_at);
    if secs < 60 {
        format!("{secs} 秒")
    } else {
        format!("{} 分", secs / 60)
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
                "description": "列出当前会话的后台 Bash 任务（id、状态、命令、耗时）。",
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
                return Ok(ToolEffect::plain("没有后台任务".to_string()));
            }
            let out = tasks
                .iter()
                .map(|entry| {
                    format!(
                        "{} [{}] {}（{}）",
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
                "description": "查看后台 Bash 任务的输出（尾部节选）。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "task_id": { "type": "string", "description": "后台任务 id（b1、b2…）" }
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
            let task_id = args["task_id"].as_str().ok_or("缺少参数 task_id")?;
            let tasks = ctx.state.tasks.lock().map_err(|e| e.to_string())?;
            let Some(entry) = tasks.iter().find(|t| t.id == task_id) else {
                return Err(format!("任务不存在: {task_id}"));
            };
            let tail = crate::task::tail_chars(&entry.output, 16000);
            let tail = if tail.is_empty() {
                "（暂无输出）".to_string()
            } else {
                tail
            };
            Ok(ToolEffect::plain(format!(
                "任务 {}（{}，{}）输出：\n{tail}",
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

    /// 杀的是本会话自己起的后台任务，风险等同会话内状态清理；
    /// Plan 模式下也允许（与 TodoList 写操作同口径）
    fn read_only(&self) -> bool {
        true
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "TaskStop",
                "description": "停止（kill）一个仍在运行的后台 Bash 任务。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "task_id": { "type": "string", "description": "后台任务 id（b1、b2…）" }
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
            let task_id = args["task_id"].as_str().ok_or("缺少参数 task_id")?;
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

/// ExitPlanMode：模型请求退出计划模式。会话层在 Plan 硬拒之前拦截并强制弹窗
/// （ZCode 同款）；工具实现只是防御性兜底，正常路径不会走到 execute。
pub(crate) struct ExitPlanModeTool;
impl Tool for ExitPlanModeTool {
    fn name(&self) -> &'static str {
        "ExitPlanMode"
    }

    /// 只读标记：Plan 硬拒只拦非只读工具，本工具由会话层的专属弹窗接管
    fn read_only(&self) -> bool {
        true
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "ExitPlanMode",
                "description": "计划写好、准备开始执行时调用：请用户确认后退出计划模式。仅在计划模式下可用。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "plan": { "type": "string", "description": "计划摘要（展示在确认弹窗里，截取前 500 字符）" }
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
        // 防御：正常路径在 session.rs 工具循环拦截，不会走到这里
        Box::pin(async move { Err("ExitPlanMode 由会话层处理".to_string()) })
    }
}

/// EnterPlanMode：模型主动进入计划模式（任务复杂/改动大时先调研出计划）。
/// 进计划是自我收紧（只读化），会话层直接切换不弹窗；工具实现只是防御性兜底。
pub(crate) struct EnterPlanModeTool;

impl Tool for EnterPlanModeTool {
    fn name(&self) -> &'static str {
        "EnterPlanMode"
    }

    /// 只读标记：免审批、Plan 下不被拦（幂等提示）
    fn read_only(&self) -> bool {
        true
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "EnterPlanMode",
                "description": "任务复杂或改动范围大时调用：进入计划模式（只读调研），计划写好后用 ExitPlanMode 请用户确认执行。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "reason": { "type": "string", "description": "为什么进入计划模式（可选，仅记录）" }
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
        // 防御：正常路径在 session.rs 工具循环拦截，不会走到这里
        Box::pin(async move { Err("EnterPlanMode 由会话层处理".to_string()) })
    }
}

/// Agent：委派子代理处理独立子任务。会话层在工具循环里拦截执行
///（run_subagent 前台同步循环），工具实现只注册 schema 与描述。
pub struct AgentTool {
    /// 可用子代理类型清单（agent_description_list 结果），拼进 description
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

    /// 非只读：让 Plan 硬拒语义成立（子代理可能修改文件）
    fn read_only(&self) -> bool {
        false
    }

    fn is_shell(&self) -> bool {
        false
    }

    fn schema(&self) -> serde_json::Value {
        let description = format!(
            "启动子代理处理任务。子代理独立运行、有自己的上下文——它看不到本会话的任何消息，prompt 必须自包含（像给刚进门的同事做简报：说清目标、已知结论、确切文件路径）。\n\
             好处：子代理的中间过程（大量文件读取/搜索）不进本会话上下文，你只收到它最后的结论。\n\
             - 查找类任务给确切路径或命令；调查类任务给问题，不给死步骤。\n\
             - 不要委派一两步就能完成的琐事；子代理运行中不要并行重做它的工作，也不要中途抛弃它自己手动完成。\n\
             - 子代理的结果只有你能看到（用户看不到），需要时自己转述。\n\
             - run_in_background=true 立即返回（带 task_id）；完成后你会收到通知，**结果全文在通知给出的文件里，用 Read 读取**，不要轮询。优先用 resume 继续已有子代理而不是新起实例。\n\
             可用子代理类型（省略 subagent_type 时默认 general-purpose）：\n\
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
                        "description": { "type": "string", "description": "3-5 词任务简述，UI 显示用" },
                        "prompt": { "type": "string", "description": "完整自包含的任务简报（子代理看不到本会话任何消息）" },
                        "subagent_type": { "type": "string", "description": "子代理类型，省略默认 general-purpose；与 resume 互斥" },
                        "run_in_background": { "type": "boolean", "description": "true 立即返回（带 task_id），子代理后台运行；完成后你会收到通知，结果全文在通知给出的文件里（用 Read 读取）" },
                        "resume": { "type": "string", "description": "已有 agent_id，在其上下文上续跑（与 subagent_type 互斥）" }
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
        // 防御：正常路径在 session.rs 工具循环拦截（run_subagent），不会走到这里
        Box::pin(async move { Err("Agent 由会话层处理".to_string()) })
    }
}

/// 解析并校验 AskUserQuestion 参数：1-4 题；每题 question 非空、options 2-4 项、
/// label 非空。纯函数以便单测；真正的请求/等待在 session.rs 工具循环拦截。
pub fn parse_questions(
    args: &serde_json::Value,
) -> Result<Vec<pig_protocol::QuestionItem>, String> {
    let items = args["questions"]
        .as_array()
        .ok_or("缺少参数 questions（数组）")?;
    if items.is_empty() || items.len() > 4 {
        return Err(format!(
            "questions 数量须在 1-4 之间（收到 {}）",
            items.len()
        ));
    }
    let mut questions = Vec::new();
    for (ix, item) in items.iter().enumerate() {
        let n = ix + 1;
        let question = item["question"].as_str().unwrap_or("").trim().to_string();
        if question.is_empty() {
            return Err(format!("第 {n} 题 question 不能为空"));
        }
        let header = item["header"]
            .as_str()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let multi_select = item["multi_select"].as_bool().unwrap_or(false);
        let options = item["options"]
            .as_array()
            .ok_or_else(|| format!("第 {n} 题缺少 options（数组）"))?;
        if options.len() < 2 || options.len() > 4 {
            return Err(format!(
                "第 {n} 题 options 数量须在 2-4 之间（收到 {}）",
                options.len()
            ));
        }
        let mut parsed_options = Vec::new();
        for option in options {
            let label = option["label"].as_str().unwrap_or("").trim().to_string();
            if label.is_empty() {
                return Err(format!("第 {n} 题存在空 label 的选项"));
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
                "description": "需要用户决策时，给出 1-4 个结构化问题（每题 2-4 个选项）让用户选择，而不是用纯文本提问。每题可用 multi_select 允许多选；UI 会自动追加「其他」自由输入项。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "questions": {
                            "type": "array",
                            "description": "1-4 个问题",
                            "minItems": 1,
                            "maxItems": 4,
                            "items": {
                                "type": "object",
                                "properties": {
                                    "question": { "type": "string", "description": "完整问题文本" },
                                    "header": { "type": "string", "description": "可选短标签（≤12 字）" },
                                    "multi_select": { "type": "boolean", "description": "可选，true 允许多选（默认 false）" },
                                    "options": {
                                        "type": "array",
                                        "description": "2-4 个选项",
                                        "minItems": 2,
                                        "maxItems": 4,
                                        "items": {
                                            "type": "object",
                                            "properties": {
                                                "label": { "type": "string", "description": "选项标签" },
                                                "description": { "type": "string", "description": "可选补充说明" }
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
        // 防御：正常路径在 session.rs 工具循环拦截，不会走到这里
        Box::pin(async move { Err("AskUserQuestion 由会话层处理".to_string()) })
    }
}
