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
             - 同一任务要批量作用于多个对象（一个模板 × N 个 item 的 fan-out）时用 AgentSwarm，一次铺开并发执行。\n\
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

/// AgentSwarm：批量并行子代理（一个 prompt 模板 × N 个 item）。schema-only，
/// 会话层在工具循环拦截后走 swarm 并发执行（agent/swarm.rs 准备 + task.rs
/// 全局并发槽），工具实现只注册 schema 与描述。
pub struct AgentSwarmTool {
    /// 可用子代理类型清单（agent_description_list 结果），拼进 description
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

    /// 非只读：让 Plan 硬拒语义成立（子代理可能修改文件）
    fn read_only(&self) -> bool {
        false
    }

    fn is_shell(&self) -> bool {
        false
    }

    fn schema(&self) -> serde_json::Value {
        let description = format!(
            "批量并行子代理：一个 prompt 模板 × N 个 item——模板里的 {{{{item}}}} 占位符被每个 item 替换后各启动一个子代理，全部并发执行（全局并发上限，超限自动排队）。\n\
             - 适用「同一任务批量作用于多个对象」（逐个审查这些文件/逐个迁移这些模块）；item 只放变化的部分（路径/名字/参数），背景与要求在模板里说全（子代理看不到本会话任何消息，prompt 必须自包含）。\n\
             - 互不相同的一两个任务请改用 Agent；展开后的 prompt 两两不得相同；items 上限 {MAX_SWARM_ITEMS}。\n\
             - resume_agent_ids 续跑已有子代理（已有 agent_id → 追加 prompt），可与 items 同用；纯续跑时 subagent_type 无意义。\n\
             - 默认前台：阻塞至全部完成，返回聚合结果（失败项带错误原因；每项结果全文在聚合结果给出的 result.md 文件里，用 Read 读取），期间取消会取消全部子代理。\n\
             - run_in_background=true 立即返回逐项回执（agent_id/task_id/状态），子代理群后台运行：每项完成或失败都会以 <task-notification> 逐个送达（结果全文在通知给出的文件里，用 Read 读取），不要轮询；后台子代理不随本会话回合取消，可用 TaskStop 逐个停止。\n\
             可用子代理类型（省略 subagent_type 时默认 general-purpose）：\n\
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
                        "prompt_template": { "type": "string", "description": "任务模板：{{item}} 占位符会被每个 item 替换；背景/要求/输出格式在这里说全" },
                        "items": {
                            "type": "array",
                            "description": "批量对象清单：每个 item 启动一个子代理（纯 items 时至少 2 个）",
                            "items": { "type": "string" }
                        },
                        "resume_agent_ids": {
                            "type": "object",
                            "description": "可选：已有 agent_id → 追加 prompt（续跑已有子代理，可与 items 同用）",
                            "additionalProperties": { "type": "string" }
                        },
                        "subagent_type": { "type": "string", "description": "子代理类型，省略默认 general-purpose；仅作用于 items 新起的子代理" },
                        "run_in_background": { "type": "boolean", "description": "true 立即返回（逐项列出 agent_id/task_id），子代理群后台运行；每项完成/失败都会以 <task-notification> 逐个送达，结果全文在通知给出的文件里（用 Read 读取），不要轮询" }
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
        // 防御：正常路径在 session.rs 工具循环拦截（run_swarm），不会走到这里
        Box::pin(async move { Err("AgentSwarm 由会话层处理".to_string()) })
    }
}

/// AgentSwarm items 上限（展开后每个 item 一个子代理）
pub const MAX_SWARM_ITEMS: usize = 128;

/// AgentSwarm 展开后的一个子任务
#[derive(Debug)]
pub struct SwarmTask {
    /// 续跑目标（已有 agent_id）；None = 新起
    pub resume: Option<String>,
    /// 子代理卡显示用的简述（item / 追加 prompt 的首行截断）
    pub description: String,
    /// 最终 prompt：items 路径已展开 {{item}}；resume 路径为追加 prompt 原文
    pub prompt: String,
}

/// AgentSwarm 解析结果（执行前的完整计划）
#[derive(Debug)]
pub struct SwarmPlan {
    pub tasks: Vec<SwarmTask>,
    /// 新起子代理的档案查询名（缺省 general-purpose；纯续跑时无意义，校验已拒）
    pub subagent_type: String,
}

impl SwarmPlan {
    /// 新起子任务数（items 展开；resume 条目不算）
    pub fn item_count(&self) -> usize {
        self.tasks.iter().filter(|t| t.resume.is_none()).count()
    }
}

/// 解析并校验 AgentSwarm 参数（纯函数以便单测；会话层拦截后执行）：
/// items 与 resume_agent_ids 至少给一类；纯 items 时 ≥2 个；items 上限
/// MAX_SWARM_ITEMS；模板必须含 {{item}} 占位符；展开后的 prompt 两两不得相同。
pub fn parse_swarm_args(args: &serde_json::Value) -> Result<SwarmPlan, String> {
    let template = args["prompt_template"]
        .as_str()
        .ok_or("缺少参数 prompt_template（含 {{item}} 占位符的任务模板）")?
        .trim()
        .to_string();
    // items：可选字符串数组；空白/非字符串元素报错（静默丢弃会让模型误判并发数）
    let mut items: Vec<String> = Vec::new();
    if !args["items"].is_null() {
        let array = args["items"]
            .as_array()
            .ok_or("参数 items 须为字符串数组")?;
        for (ix, item) in array.iter().enumerate() {
            let item = item
                .as_str()
                .ok_or_else(|| format!("items 第 {} 个元素须为字符串", ix + 1))?
                .trim();
            if item.is_empty() {
                return Err(format!("items 第 {} 个元素不能为空", ix + 1));
            }
            items.push(item.to_string());
        }
    }
    // resume_agent_ids：可选对象 map（agent_id → 追加 prompt）；下方显式按键排序，
    // 任务顺序确定（serde_json Map 迭代序受 preserve_order feature 影响，不可依赖）
    let mut resumes: Vec<(String, String)> = Vec::new();
    if !args["resume_agent_ids"].is_null() {
        let map = args["resume_agent_ids"]
            .as_object()
            .ok_or("参数 resume_agent_ids 须为对象（agent_id → 追加 prompt）")?;
        // serde_json Map 的迭代序受 preserve_order feature 影响（workspace 统一编译时
        // 可能是插入序而非键序）——显式排序，保证任务顺序确定
        let mut entries: Vec<(&String, &serde_json::Value)> = map.iter().collect();
        entries.sort_by_key(|(agent_id, _)| agent_id.as_str());
        for (agent_id, prompt) in entries {
            let agent_id = agent_id.trim();
            let prompt = prompt
                .as_str()
                .ok_or_else(|| format!("resume_agent_ids[\"{agent_id}\"] 须为字符串"))?
                .trim();
            if agent_id.is_empty() {
                return Err("resume_agent_ids 存在空 agent_id 键".to_string());
            }
            if prompt.is_empty() {
                return Err(format!(
                    "resume_agent_ids[\"{agent_id}\"] 的追加 prompt 不能为空"
                ));
            }
            resumes.push((agent_id.to_string(), prompt.to_string()));
        }
    }
    if items.is_empty() && resumes.is_empty() {
        return Err("items 与 resume_agent_ids 至少给一类".to_string());
    }
    if resumes.is_empty() && items.len() < 2 {
        return Err("纯 items 批量至少 2 个 item（单个任务请用 Agent 工具）".to_string());
    }
    if items.len() > MAX_SWARM_ITEMS {
        return Err(format!(
            "items 总数上限 {MAX_SWARM_ITEMS}（收到 {}）",
            items.len()
        ));
    }
    let subagent_type = args["subagent_type"].as_str().unwrap_or("").trim();
    if items.is_empty() && !subagent_type.is_empty() {
        return Err(
            "纯 resume 续跑时 subagent_type 无意义（续跑按各自档案现状重解析）".to_string(),
        );
    }
    let mut tasks: Vec<SwarmTask> = Vec::new();
    if !items.is_empty() {
        if template.is_empty() {
            return Err("prompt_template 不能为空".to_string());
        }
        if !template.contains("{{item}}") {
            return Err(
                "prompt_template 缺少 {{item}} 占位符（每个 item 会替换进模板）".to_string(),
            );
        }
        // 展开后两两去重：重复 prompt = 重复 item（占位符存在性已在上方校验）
        let mut seen = std::collections::HashSet::new();
        for item in &items {
            let prompt = template.replace("{{item}}", item);
            if !seen.insert(prompt.clone()) {
                return Err(format!(
                    "展开后的 prompt 重复：item \"{}\" 与其他 item 展开结果相同（检查是否有重复 item）",
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

/// 子代理卡简述：首行截断 60 字符（item / 追加 prompt 可能很长）
fn swarm_task_description(text: &str) -> String {
    let first_line = text.lines().next().unwrap_or("").trim();
    first_line.chars().take(60).collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    // ---------- parse_swarm_args ----------

    #[test]
    fn swarm_expand_items() {
        let plan = parse_swarm_args(&serde_json::json!({
            "prompt_template": "审查 {{item}}，输出问题清单",
            "items": ["src/a.rs", "src/b.rs"]
        }))
        .expect("合法调用");
        assert_eq!(plan.tasks.len(), 2);
        assert_eq!(plan.tasks[0].prompt, "审查 src/a.rs，输出问题清单");
        assert_eq!(plan.tasks[1].prompt, "审查 src/b.rs，输出问题清单");
        assert!(
            plan.tasks.iter().all(|t| t.resume.is_none()),
            "items 路径不带 resume"
        );
        assert_eq!(plan.subagent_type, "general-purpose", "缺省档案");
        assert_eq!(plan.item_count(), 2);
        // 多处占位符全部替换
        let plan = parse_swarm_args(&serde_json::json!({
            "prompt_template": "读 {{item}} 并比对 {{item}} 的调用方",
            "items": ["a", "b"]
        }))
        .expect("多占位符");
        assert_eq!(plan.tasks[0].prompt, "读 a 并比对 a 的调用方");
    }

    #[test]
    fn swarm_missing_placeholder_err() {
        let err = parse_swarm_args(&serde_json::json!({
            "prompt_template": "审查这些文件",
            "items": ["a", "b"]
        }))
        .unwrap_err();
        assert!(err.contains("{{item}}"), "应点名缺占位符: {err}");
    }

    #[test]
    fn swarm_duplicate_expansion_err() {
        let err = parse_swarm_args(&serde_json::json!({
            "prompt_template": "审查 {{item}}",
            "items": ["a.rs", "a.rs"]
        }))
        .unwrap_err();
        assert!(err.contains("重复"), "重复 item 应拒绝: {err}");
        // item 首尾空白 trim 后展开相同，同样算重复
        let err = parse_swarm_args(&serde_json::json!({
            "prompt_template": "审查 {{item}}",
            "items": ["a.rs", "  a.rs  "]
        }))
        .unwrap_err();
        assert!(err.contains("重复"), "trim 后重复也应拒绝: {err}");
    }

    #[test]
    fn swarm_items_limit() {
        let items: Vec<String> = (0..MAX_SWARM_ITEMS).map(|i| format!("f{i}")).collect();
        let plan = parse_swarm_args(&serde_json::json!({
            "prompt_template": "处理 {{item}}",
            "items": items
        }))
        .expect("恰好上限应通过");
        assert_eq!(plan.tasks.len(), MAX_SWARM_ITEMS);
        let items: Vec<String> = (0..=MAX_SWARM_ITEMS).map(|i| format!("f{i}")).collect();
        let err = parse_swarm_args(&serde_json::json!({
            "prompt_template": "处理 {{item}}",
            "items": items
        }))
        .unwrap_err();
        assert!(err.contains("上限"), "超上限应报错: {err}");
    }

    #[test]
    fn swarm_shape_rules() {
        // 两类都不给
        let err =
            parse_swarm_args(&serde_json::json!({"prompt_template": "x {{item}}"})).unwrap_err();
        assert!(err.contains("至少给一类"), "{err}");
        // 纯 items 单元素
        let err = parse_swarm_args(&serde_json::json!({
            "prompt_template": "x {{item}}",
            "items": ["only"]
        }))
        .unwrap_err();
        assert!(err.contains("至少 2"), "{err}");
        // 空 item / 非字符串 item
        assert!(
            parse_swarm_args(&serde_json::json!({
                "prompt_template": "x {{item}}",
                "items": ["a", "  "]
            }))
            .is_err(),
            "空白 item 应拒绝"
        );
        assert!(
            parse_swarm_args(&serde_json::json!({
                "prompt_template": "x {{item}}",
                "items": ["a", 1]
            }))
            .is_err(),
            "非字符串 item 应拒绝"
        );
        // 空模板 + items
        assert!(
            parse_swarm_args(&serde_json::json!({
                "prompt_template": "  ",
                "items": ["a", "b"]
            }))
            .is_err(),
            "空模板应拒绝"
        );
    }

    #[test]
    fn swarm_resume_rules() {
        // 纯 resume 合法（prompt_template schema 必填，此处被忽略）
        let plan = parse_swarm_args(&serde_json::json!({
            "prompt_template": "",
            "resume_agent_ids": {"a2": "再查一下", "a1": "继续"}
        }))
        .expect("纯 resume");
        assert_eq!(plan.tasks.len(), 2);
        assert_eq!(
            plan.tasks[0].resume.as_deref(),
            Some("a1"),
            "对象 map 按键序迭代"
        );
        assert_eq!(plan.item_count(), 0);
        // 纯 resume + subagent_type → 报错
        assert!(
            parse_swarm_args(&serde_json::json!({
                "prompt_template": "",
                "resume_agent_ids": {"a1": "继续"},
                "subagent_type": "explore"
            }))
            .is_err(),
            "纯续跑给 subagent_type 应拒绝"
        );
        // items + resume 混用：单 item 合法，items 排前
        let plan = parse_swarm_args(&serde_json::json!({
            "prompt_template": "处理 {{item}}",
            "items": ["one"],
            "resume_agent_ids": {"a1": "继续"},
            "subagent_type": "explore"
        }))
        .expect("混用单 item");
        assert_eq!(plan.tasks.len(), 2);
        assert!(plan.tasks[0].resume.is_none(), "items 子任务排前");
        assert_eq!(plan.tasks[1].resume.as_deref(), Some("a1"));
        assert_eq!(plan.subagent_type, "explore");
        // 空追加 prompt
        assert!(
            parse_swarm_args(&serde_json::json!({
                "prompt_template": "",
                "resume_agent_ids": {"a1": "  "}
            }))
            .is_err(),
            "空追加 prompt 应拒绝"
        );
    }

    #[test]
    fn swarm_description_first_line_truncated() {
        let long = format!("{}\n第二行不要", "字".repeat(100));
        let plan = parse_swarm_args(&serde_json::json!({
            "prompt_template": "处理 {{item}}",
            "items": [long, "短"]
        }))
        .expect("长 item");
        assert_eq!(
            plan.tasks[0].description.chars().count(),
            60,
            "简述取首行截 60 字符"
        );
        assert_eq!(plan.tasks[1].description, "短");
    }

    #[test]
    fn swarm_run_in_background_not_in_plan() {
        // run_in_background 是执行方式开关，不进 SwarmPlan（校验规则不受影响）
        let with = parse_swarm_args(&serde_json::json!({
            "prompt_template": "审查 {{item}}",
            "items": ["a.rs", "b.rs"],
            "run_in_background": true
        }))
        .expect("带后台开关的合法调用");
        let without = parse_swarm_args(&serde_json::json!({
            "prompt_template": "审查 {{item}}",
            "items": ["a.rs", "b.rs"]
        }))
        .expect("不带后台开关的合法调用");
        assert_eq!(with.tasks.len(), without.tasks.len());
        for (a, b) in with.tasks.iter().zip(without.tasks.iter()) {
            assert_eq!(a.prompt, b.prompt);
            assert_eq!(a.description, b.description);
            assert_eq!(a.resume, b.resume);
        }
        assert_eq!(with.subagent_type, without.subagent_type);
    }
}
