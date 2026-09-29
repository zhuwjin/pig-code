//! AgentSwarm 批量并行子代理的执行侧：子任务上下文准备（复用 run_subagent 同款
//! 档案/模型/工具收窄/JSONL 落盘管线）与聚合结果格式化。参数展开/校验在
//! tool::parse_swarm_args（schema 层纯函数）；并发驱动在 session 层
//!（drive_subagent 私有），全局并发槽在 task.rs。

use super::*;
use crate::provider::ChatMsg;

/// 聚合结果总预算（与单个子代理结果注入父会话的 32K 预算一致）
pub const SWARM_RESULT_BUDGET: usize = 32_000;
/// 单个子代理在聚合结果里的预览上限（超出截断，全文见各自 result.md）
pub const SWARM_CHILD_PREVIEW: usize = 3_000;

/// AgentSwarm 单个子代理的最终状态
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SwarmChildStatus {
    Completed,
    Failed,
    /// 父 turn 取消 / TaskStop
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

/// 聚合用的单子代理结果（session 层驱动完成后构造；准备阶段失败无 agent_id）
pub struct SwarmChildResult {
    pub description: String,
    pub agent_id: Option<String>,
    pub status: SwarmChildStatus,
    pub turns: usize,
    /// 结果全文路径（result.md 落盘成功且非取消时）
    pub result_path: Option<PathBuf>,
    /// 结果文本（驱动已按 32K 截断；聚合时再加 SWARM_CHILD_PREVIEW 上限）
    pub result_text: String,
    /// 是否经历过全局并发槽排队（聚合头部说明排队情况用）
    pub queued: bool,
    /// token 用量 (input, cache_read, output)：前台 swarm 累加进父回合统计
    pub usage: (u64, u64, u64),
}

/// 聚合结果为单个工具结果：头部统计（成功/失败/取消 + 并发槽排队情况），
/// 每个子任务一段（简述、agent_id、状态、结果文件指针、预览）。总长按
/// SWARM_RESULT_BUDGET 截断——溢出段降级为指针行（每项仍可达 result.md）。
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
            "；全局并发上限 {}，其中 {queued} 个曾排队等待空槽",
            crate::task::MAX_CONCURRENT_SUBAGENTS
        )
    } else {
        String::new()
    };
    let mut out = format!(
        "子代理群执行完成：共 {total} 个（成功 {completed} / 失败 {failed} / 取消 {cancelled}）{queue_note}。\n\
         每项结果全文在下方给出的 result.md 文件里，需要完整内容用 Read 读取；用 Agent(resume=\"agent_id\", prompt=\"...\") 可续跑某一项。\n"
    );
    for (ix, child) in children.iter().enumerate() {
        let section = format_child_section(ix + 1, child);
        if out.chars().count() + section.chars().count() > SWARM_RESULT_BUDGET {
            // 预算耗尽：本段及后续全部降级为指针行；指针尾必须完整保留
            //（每项的状态与 result.md 都可达），头部为它腾空间
            let mut tail = String::from("\n[聚合结果过长，以下子代理只留状态与结果文件指针：]\n");
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

/// 聚合结果里的单子任务段：简述标题 + agent_id/状态/轮次 + 结果文件指针 + 预览
fn format_child_section(n: usize, child: &SwarmChildResult) -> String {
    let mut section = format!("\n## {n}. {}\n", child.description);
    match &child.agent_id {
        Some(agent_id) => section.push_str(&format!(
            "agent_id: {agent_id} · status: {} · turns: {}\n",
            child.status.label(),
            child.turns
        )),
        None => section.push_str(&format!(
            "status: {}（准备阶段失败，未启动）\n",
            child.status.label()
        )),
    }
    if let Some(path) = &child.result_path {
        section.push_str(&format!("结果全文: {}\n", path.display()));
    }
    let count = child.result_text.chars().count();
    if count == 0 {
        let placeholder = match child.status {
            SwarmChildStatus::Cancelled => "（已停止，无结果）",
            _ => "（无结果文本）",
        };
        section.push_str(placeholder);
    } else if count > SWARM_CHILD_PREVIEW {
        let preview: String = child
            .result_text
            .chars()
            .take(SWARM_CHILD_PREVIEW)
            .collect();
        let hint = match &child.result_path {
            Some(path) => format!("\n[预览截断，全文: {}]", path.display()),
            None => "\n[预览截断]".to_string(),
        };
        section.push_str(&preview);
        section.push_str(&hint);
    } else {
        section.push_str(&child.result_text);
    }
    section.push('\n');
    section
}

/// 预算溢出时的单行指针（description 已截 60 字符，行长远小于预算分摊）
fn child_pointer_line(child: &SwarmChildResult) -> String {
    let base = match &child.agent_id {
        Some(agent_id) => format!(
            "- {}（{agent_id}，status: {}）",
            child.description,
            child.status.label()
        ),
        None => format!(
            "- {}（status: {}，准备阶段失败）",
            child.description,
            child.status.label()
        ),
    };
    match &child.result_path {
        Some(path) => format!("{base}：{}", path.display()),
        None => base,
    }
}

/// 一个子代理的完整执行准备（SubagentDrive 的全部原料，session 层组装驱动）
pub struct SwarmChildPrep {
    pub agent_id: String,
    pub profile: AgentProfile,
    pub child_config: ResolvedModel,
    pub tools: Vec<Box<dyn crate::tool::Tool>>,
    pub schemas: Vec<serde_json::Value>,
    pub history: Vec<ChatMsg>,
    pub jsonl: PathBuf,
    pub max_turns: usize,
    pub description: String,
    /// 会话 MCP 句柄（None = 未连接）：并发驱动闭包在 GateCtx 组装点现取继承工具
    pub mcp: Option<std::sync::Arc<crate::mcp::McpManager>>,
    /// 继承规则快照（与 run_subagent 同口径）
    pub mcp_inherits_all: bool,
}

/// 单个子任务的准备结果：失败不影响其他子任务（聚合时记为准备阶段失败项）
pub enum SwarmPrep {
    Ready(Box<SwarmChildPrep>),
    Failed { description: String, error: String },
}

/// 后台 swarm 即时回执里的一项（已派发的 Ready 子代理，或准备阶段失败项）
pub struct SwarmReceiptChild {
    pub description: String,
    /// 已派发子代理的 agent_id（None = 准备阶段失败未启动，error 带原因）
    pub agent_id: Option<String>,
    /// 已派发子代理的后台任务 task_id（与 agent_id 同有无）
    pub task_id: Option<String>,
    /// 回执组装瞬间按空闲并发槽估算的排队态（true = 排队等待空槽；瞬时值，仅供参考）
    pub queued: bool,
    /// 准备阶段失败原因
    pub error: Option<String>,
}

/// 后台 swarm 的即时回执（纯函数以便单测）：头部统计 + 逐项 agent_id/task_id/status
/// （queued/running），准备阶段失败项同样列出；告知完成经 <task-notification>
/// 逐个送达、不要轮询（与单后台 Agent 回执同口径）。
pub fn format_swarm_receipt(children: &[SwarmReceiptChild]) -> String {
    let total = children.len();
    let queued = children.iter().filter(|c| c.queued).count();
    let failed = children.iter().filter(|c| c.agent_id.is_none()).count();
    let mut notes = String::new();
    if queued > 0 {
        notes.push_str(&format!(
            "；全局并发上限 {}，其中 {queued} 个先排队等待空槽",
            crate::task::MAX_CONCURRENT_SUBAGENTS
        ));
    }
    if failed > 0 {
        notes.push_str(&format!("；{failed} 个准备阶段失败（未启动，见下方列表）"));
    }
    let mut out = format!(
        "子代理群已在后台启动：共 {total} 个{notes}。\n\
         每项完成或失败都会以 <task-notification> 逐个送达——不要轮询；结果全文在通知给出的文件里（用 Read 读取）。\n\
         可用 TaskList 查看、TaskOutput 看进度、TaskStop 停止、Agent(resume=\"agent_id\", prompt=\"...\") 续跑某一项。\n"
    );
    for (ix, child) in children.iter().enumerate() {
        out.push_str(&format!("\n{}. {}\n", ix + 1, child.description));
        match (&child.agent_id, &child.task_id) {
            (Some(agent_id), Some(task_id)) => out.push_str(&format!(
                "agent_id: {agent_id} · task_id: {task_id} · status: {}\n",
                if child.queued { "queued" } else { "running" }
            )),
            _ => out.push_str(&format!(
                "status: failed（准备阶段失败，未启动）: {}\n",
                child.error.as_deref().unwrap_or("未知错误")
            )),
        }
    }
    out
}

/// prepare_swarm_children 的入参包（Session 字段快照，全不可变借用）
pub struct SwarmPrepCtx<'a> {
    pub cwd: &'a Path,
    pub data_dir: &'a Path,
    pub git_snapshot: Option<&'a str>,
    pub app_config: Option<&'a AppConfig>,
    pub parent_config: &'a ResolvedModel,
    pub session_id: &'a str,
    /// resume 条目的运行中冲突检测（注册表同 agent_id 且 Running → 拒绝并行续跑）
    pub tasks: &'a crate::task::TaskRegistry,
    /// 会话 MCP 句柄（None = 未连接）：MCP 工具按继承规则进子代理工具集
    pub mcp: Option<&'a std::sync::Arc<crate::mcp::McpManager>>,
}

/// 批量准备子代理上下文（与 run_subagent 同管线：档案/模型/工具收窄/JSONL 落盘）。
/// items 子任务共用一份档案+模型解析——档案/模型错误是调用级参数错误，整体 Err
/// 当场暴露（resolve_subagent_model 严格语义）；resume 条目各自按现状重解析，
/// 单条失败（不存在/运行中/档案已删）只记该条 Failed，不影响其他。
pub fn prepare_swarm_children(
    ctx: &SwarmPrepCtx<'_>,
    plan: &crate::tool::SwarmPlan,
    agent_seq: &mut u64,
) -> Result<Vec<SwarmPrep>, String> {
    let profiles = load_profiles(ctx.cwd, ctx.data_dir);
    let agents_dir = agents_dir(&ctx.data_dir.join("sessions"), ctx.session_id);
    // items 共用档案/模型：有 items 才解析（纯续跑不碰 subagent_type）
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
                let profile = item_profile.clone().expect("item 子任务必有档案");
                let child_config = item_config.clone().expect("item 子任务必有模型");
                *agent_seq += 1;
                let agent_id = format!("a{}-{}", crate::rollout::now_secs(), *agent_seq);
                let history = vec![
                    ChatMsg::system(crate::prompt::subagent_system_prompt(
                        &profile,
                        ctx.cwd,
                        ctx.data_dir,
                        ctx.git_snapshot,
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
                    ctx.mcp,
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

/// resume 子任务准备：读入已有上下文 + 追加新 prompt（档案/模型按现状重解析，
/// 与 run_subagent 的 resume 分支同口径）；模型解析失败不持久化追加行。
fn prepare_resume(
    ctx: &SwarmPrepCtx<'_>,
    profiles: &[AgentProfile],
    agents_dir: &Path,
    resume_id: &str,
    task: &crate::tool::SwarmTask,
) -> Result<SwarmChildPrep, String> {
    let jsonl = agents_dir.join(format!("{resume_id}.jsonl"));
    if !jsonl.exists() {
        // 列可用 agent_id（*.jsonl 去后缀），帮助模型纠正拼写
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
            "（无）".to_string()
        } else {
            ids.join(", ")
        };
        return Err(format!("子代理 \"{resume_id}\" 不存在。可用: {available}"));
    }
    // 运行中冲突：注册表里同 agent_id 且 Running → 不允许并行续跑
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
            "该子代理仍在运行（task_id {task_id}），可用 TaskStop 停止后再续跑"
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
        ctx.mcp,
    ))
}

/// 子代理模型解析（严格：失败即报错；未加载应用配置时显式模型不可用，继承不受影响）
fn resolve_child_config(
    ctx: &SwarmPrepCtx<'_>,
    profile: &AgentProfile,
) -> Result<ResolvedModel, String> {
    match ctx.app_config {
        Some(app_config) => resolve_subagent_model(app_config, ctx.parent_config, profile),
        None if profile.model.is_some() => {
            Err("未加载应用配置，无法解析子代理指定模型".to_string())
        }
        None => Ok(ctx.parent_config.clone()),
    }
}

/// 组装执行准备：工具收窄（档案列表 ∩ 全部 − FORBIDDEN；input_image=false 再剔
/// ReadMediaFile）+ MCP 继承（与 run_subagent 同规则：全工具档案继承全部已连接
/// MCP 工具，只读档案只继承 readOnlyHint 的）+ max_turns 缺省
fn assemble_prep(
    agent_id: String,
    profile: AgentProfile,
    child_config: ResolvedModel,
    history: Vec<ChatMsg>,
    jsonl: PathBuf,
    description: String,
    mcp: Option<&std::sync::Arc<crate::mcp::McpManager>>,
) -> SwarmChildPrep {
    let all_tools = crate::tool::all();
    let all_names: Vec<String> = all_tools.iter().map(|t| t.name().to_string()).collect();
    let keep = child_tool_set(&profile, &all_names, child_config.input_image);
    let mut tools: Vec<Box<dyn crate::tool::Tool>> = all_tools
        .into_iter()
        .filter(|t| keep.iter().any(|name| name == t.name()))
        .collect();
    let mcp_inherits_all = child_inherits_all_mcp(&keep);
    if let Some(mcp) = mcp {
        tools.extend(mcp.child_tools(mcp_inherits_all));
    }
    let schemas: Vec<serde_json::Value> = tools.iter().map(|t| t.schema()).collect();
    SwarmChildPrep {
        max_turns: profile.max_turns.unwrap_or(DEFAULT_MAX_TURNS),
        agent_id,
        profile,
        child_config,
        tools,
        schemas,
        history,
        jsonl,
        description,
        mcp: mcp.cloned(),
        mcp_inherits_all,
    }
}

/// 子代理上下文 JSONL 追加一行（失败非致命：打日志继续，与 rollout.append 同口径）
fn persist_line(jsonl: &Path, line: &serde_json::Value) {
    if let Err(error) = append_agent_record(jsonl, line) {
        eprintln!("[agent] 子代理上下文落盘失败: {error}");
    }
}

/// 子代理消息落盘：base64 不落盘（与主 rollout 同口径）
fn persist_msg(jsonl: &Path, msg: &ChatMsg) {
    let mut msg = msg.clone();
    msg.images.clear();
    persist_line(jsonl, &serde_json::json!({ "type": "msg", "msg": msg }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use pig_protocol::ApiFormat;

    /// 临时目录：进程号+纳秒保证唯一，Drop 自动清理
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
            std::fs::create_dir_all(&path).expect("创建临时目录");
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
            cap_web_search: false,
            web_search_tool: None,
            input_image: true,
            provider_name: "父供应商".into(),
        }
    }

    /// items 批量调用的准备上下文（cwd/data_dir 都在临时目录下）
    fn prep_ctx<'a>(
        tmp: &'a TempDir,
        tasks: &'a crate::task::TaskRegistry,
        parent: &'a ResolvedModel,
    ) -> SwarmPrepCtx<'a> {
        SwarmPrepCtx {
            cwd: &tmp.0,
            data_dir: &tmp.0,
            git_snapshot: None,
            app_config: None,
            parent_config: parent,
            session_id: "s-test",
            tasks,
            mcp: None,
        }
    }

    /// 带 MCP 句柄的假工具规格
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
            "prompt_template": "审查 {{item}}",
            "items": ["a.rs", "b.rs"]
        }))
        .expect("合法调用");
        let mut seq = 0u64;
        let preps = prepare_swarm_children(&prep_ctx(&tmp, &tasks, &parent), &plan, &mut seq)
            .expect("内置档案应解析成功");
        assert_eq!(preps.len(), 2);
        assert_eq!(seq, 2, "每个 item 子任务消耗一个序号");
        let mut agent_ids = Vec::new();
        for prep in &preps {
            let SwarmPrep::Ready(prep) = prep else {
                panic!("items 子任务应全部 Ready");
            };
            agent_ids.push(prep.agent_id.clone());
            assert_eq!(prep.profile.name, "general-purpose");
            assert_eq!(prep.child_config.model, "parent-model", "继承父模型");
            assert_eq!(prep.history.len(), 2, "system + user");
            assert!(
                prep.history[1]
                    .content
                    .as_deref()
                    .is_some_and(|c| c.contains(".rs")),
                "user 消息是展开后的 prompt"
            );
            let names: Vec<&str> = prep.tools.iter().map(|t| t.name()).collect();
            assert!(names.contains(&"Write"), "general-purpose 全工具");
            for banned in ["Agent", "AgentSwarm", "AskUserQuestion", "EnterPlanMode"] {
                assert!(!names.contains(&banned), "嵌套工具必须剔除: {banned}");
            }
            // JSONL 落盘：meta 行 + 两条消息，可读回
            let (meta, history) = read_agent(&prep.jsonl).expect("落盘上下文可读回");
            assert_eq!(meta.profile, "general-purpose");
            assert_eq!(history.len(), 2);
        }
        assert_ne!(agent_ids[0], agent_ids[1], "agent_id 必须唯一");
    }

    #[test]
    fn prepare_bad_subagent_type_fails_whole_call() {
        let tmp = TempDir::new("bad-type");
        let tasks = crate::task::TaskRegistry::default();
        let parent = parent_model();
        let plan = crate::tool::parse_swarm_args(&serde_json::json!({
            "prompt_template": "审查 {{item}}",
            "items": ["a.rs", "b.rs"],
            "subagent_type": "不存在"
        }))
        .expect("参数校验不查档案");
        let mut seq = 0u64;
        let err = prepare_swarm_children(&prep_ctx(&tmp, &tasks, &parent), &plan, &mut seq)
            .err()
            .expect("档案错误整体报错");
        assert!(err.contains("不存在"), "档案错误整体报错: {err}");
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
            "prompt_template": "审查 {{item}}",
            "items": ["a.rs", "b.rs"]
        }))
        .expect("合法调用");
        let mut ctx = prep_ctx(&tmp, &tasks, &parent);
        ctx.mcp = Some(&mcp);
        let mut seq = 0u64;
        let preps = prepare_swarm_children(&ctx, &plan, &mut seq).expect("准备成功");
        let SwarmPrep::Ready(prep) = &preps[0] else {
            panic!("应 Ready");
        };
        let names: Vec<&str> = prep.tools.iter().map(|t| t.name()).collect();
        // general-purpose（全工具档案）：继承全部已连接 MCP 工具，schemas 同步
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

        // explore（只读档案）：只继承 readOnlyHint 的 MCP 工具
        let plan = crate::tool::parse_swarm_args(&serde_json::json!({
            "prompt_template": "调研 {{item}}",
            "items": ["a.rs", "b.rs"],
            "subagent_type": "explore"
        }))
        .expect("合法调用");
        let preps = prepare_swarm_children(&ctx, &plan, &mut seq).expect("准备成功");
        let SwarmPrep::Ready(prep) = &preps[0] else {
            panic!("应 Ready");
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
        // 注册一个同 agent_id 的 Running 任务 → resume 冲突；上下文文件只需存在
        //（冲突检测在读入之前）
        let agents = agents_dir(&tmp.0.join("sessions"), "s-test");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(agents.join("a1.jsonl"), "").unwrap();
        tasks.lock().unwrap().push(crate::task::TaskEntry {
            id: "b1".into(),
            command: "子代理".into(),
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
            "prompt_template": "处理 {{item}}",
            "items": ["one"],
            "resume_agent_ids": {"a1": "继续"}
        }))
        .expect("混用合法");
        let mut seq = 0u64;
        let preps = prepare_swarm_children(&prep_ctx(&tmp, &tasks, &parent), &plan, &mut seq)
            .expect("单条失败不拖垮整体");
        assert_eq!(preps.len(), 2);
        assert!(
            matches!(&preps[0], SwarmPrep::Ready(_)),
            "item 子任务不受影响"
        );
        let SwarmPrep::Failed { error, .. } = &preps[1] else {
            panic!("冲突的 resume 条目应 Failed");
        };
        assert!(error.contains("仍在运行"), "{error}");
    }

    #[test]
    fn prepare_resume_appends_prompt() {
        let tmp = TempDir::new("resume");
        let tasks = crate::task::TaskRegistry::default();
        let parent = parent_model();
        let agents = agents_dir(&tmp.0.join("sessions"), "s-test");
        std::fs::create_dir_all(&agents).unwrap();
        // 合法上下文：meta + 一条 user
        append_agent_record(
            &agents.join("a9.jsonl"),
            &serde_json::json!({
                "type": "meta",
                "agent_id": "a9",
                "profile": "general-purpose",
                "description": "旧任务",
                "model": "m",
                "provider": "p",
                "created_at": 1,
            }),
        )
        .unwrap();
        append_agent_record(
            &agents.join("a9.jsonl"),
            &serde_json::json!({ "type": "msg", "msg": ChatMsg::user("旧 prompt".to_string()) }),
        )
        .unwrap();
        let plan = crate::tool::parse_swarm_args(&serde_json::json!({
            "prompt_template": "",
            "resume_agent_ids": {"a9": "继续查调用方"}
        }))
        .expect("纯 resume");
        let mut seq = 0u64;
        let preps =
            prepare_swarm_children(&prep_ctx(&tmp, &tasks, &parent), &plan, &mut seq).unwrap();
        assert_eq!(seq, 0, "resume 不消耗序号");
        let [SwarmPrep::Ready(prep)] = preps.as_slice() else {
            panic!("单条 resume 应 Ready");
        };
        assert_eq!(prep.agent_id, "a9", "resume 复用原 agent_id");
        assert_eq!(prep.history.len(), 2, "旧 user + 新 user");
        assert_eq!(
            prep.history[1].content.as_deref(),
            Some("继续查调用方"),
            "追加 prompt 进历史"
        );
        let (_, history) = read_agent(&prep.jsonl).unwrap();
        assert_eq!(history.len(), 2, "追加行已落盘");
    }

    // ---------- format_swarm_result ----------

    fn child(status: SwarmChildStatus, text: &str) -> SwarmChildResult {
        SwarmChildResult {
            description: "任务项".into(),
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
        let mut failed = child(SwarmChildStatus::Failed, "模型请求失败: boom");
        failed.queued = true;
        let out = format_swarm_result(&[
            child(SwarmChildStatus::Completed, "结论一"),
            failed,
            child(SwarmChildStatus::Cancelled, ""),
        ]);
        assert!(out.contains("共 3 个（成功 1 / 失败 1 / 取消 1）"), "{out}");
        assert!(
            out.contains("并发上限") && out.contains("1 个曾排队"),
            "排队情况应写进头部: {out}"
        );
        assert!(out.contains("## 1."), "每项一段");
        assert!(out.contains("结果全文: /tmp/agents/a1-1.result.md"));
        assert!(out.contains("（已停止，无结果）"), "取消项占位");
        assert!(out.contains("模型请求失败: boom"), "失败项带原因");
    }

    #[test]
    fn aggregate_preview_capped_with_pointer() {
        let big = "x".repeat(SWARM_CHILD_PREVIEW + 500);
        let out = format_swarm_result(&[child(SwarmChildStatus::Completed, &big)]);
        assert!(
            out.contains("[预览截断，全文: /tmp/agents/a1-1.result.md]"),
            "{out}"
        );
        let body = out.matches('x').count();
        assert_eq!(body, SWARM_CHILD_PREVIEW, "预览按上限截断");
    }

    #[test]
    fn aggregate_overflow_degrades_to_pointer_tail() {
        // 多项大结果撑爆总预算：溢出段降级为指针行，每项仍可达 result.md
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
            "总长受预算约束: {}",
            out.chars().count()
        );
        assert!(out.contains("只留状态与结果文件指针"), "{out}");
        assert!(out.contains("a1-39.result.md"), "尾部项的指针必须完整保留");
    }

    #[test]
    fn aggregate_prep_failure_section() {
        let prep_failed = SwarmChildResult {
            description: "续跑旧代理".into(),
            agent_id: None,
            status: SwarmChildStatus::Failed,
            turns: 0,
            result_path: None,
            result_text: "子代理 \"a9\" 不存在。可用: （无）".into(),
            queued: false,
            usage: (0, 0, 0),
        };
        let out = format_swarm_result(&[prep_failed]);
        assert!(out.contains("准备阶段失败，未启动"), "{out}");
        assert!(out.contains("不存在"), "错误原因内联: {out}");
    }

    // ---------- format_swarm_receipt（后台回执） ----------

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
            receipt_child("审查 a.rs", Some(("a1-1", "b1")), false, None),
            receipt_child("审查 b.rs", Some(("a1-2", "b2")), true, None),
            receipt_child("续跑旧代理", None, false, Some("子代理 \"a9\" 不存在")),
        ]);
        assert!(out.contains("共 3 个"), "{out}");
        assert!(out.contains("排队等待空槽"), "排队情况进头部: {out}");
        assert!(out.contains("1 个准备阶段失败"), "失败计数进头部: {out}");
        assert!(out.contains("<task-notification>"), "告知通知语义: {out}");
        assert!(out.contains("不要轮询"), "{out}");
        assert!(out.contains("1. 审查 a.rs"), "逐项列出: {out}");
        assert!(
            out.contains("agent_id: a1-1 · task_id: b1 · status: running"),
            "{out}"
        );
        assert!(
            out.contains("agent_id: a1-2 · task_id: b2 · status: queued"),
            "{out}"
        );
        assert!(
            out.contains("status: failed（准备阶段失败，未启动）: 子代理 \"a9\" 不存在"),
            "准备失败项带原因: {out}"
        );
    }

    #[test]
    fn receipt_all_running_no_queue_no_failure_notes() {
        let out = format_swarm_receipt(&[
            receipt_child("任务一", Some(("a1-1", "b1")), false, None),
            receipt_child("任务二", Some(("a1-2", "b2")), false, None),
        ]);
        assert!(out.contains("共 2 个。"), "无附加说明时头部干净: {out}");
        assert!(!out.contains("排队"), "{out}");
        assert!(!out.contains("准备阶段失败"), "{out}");
    }
}
