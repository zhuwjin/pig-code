use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use futures_util::StreamExt as _;
use futures_util::stream::FuturesUnordered;
use pig_protocol::{ApprovalDecision, Event, ExecMode, Op, SessionMeta};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::config;
use crate::paths::normalize_workspace_path;
use crate::provider::ResolvedModel;
use crate::provider::{ChatMsg, ProviderEvent, ToolCall};
use crate::rollout::{Rollout, RolloutRecord, now_secs, rebuild_history};
use crate::store::Store;
use crate::tool::{ChangeTracker, ToolContext};
use crate::{prompt, provider, tool};
use pig_protocol::AppConfig;

/// 等待中的审批：request_id → 回执通道。manager 与各 session 共享；
/// request_id 带 session_id 前缀，全局唯一。
/// 审批合并键：(工具名, 完整命令/路径, 是否危险弹窗)。并发等待者里键相同的
/// 共享一笔决议（Swarm 十个子代理同跑 `sleep 5` 只答一次）；完整命令入键而非
/// 首词，避免放行 `sleep 5` 连带放行 `sleep 100` 这类同首词不同命令。
pub type ApprovalCoalesceKey = (String, String, bool);
/// 审批等待表：request_id → (回复通道, 合并键)；键为 None 的请求（ExitPlanMode
/// 计划确认）不参与合并，只按自身 request_id 决议。
pub type PendingApprovals = Arc<
    Mutex<
        HashMap<
            String,
            (
                oneshot::Sender<ApprovalDecision>,
                Option<ApprovalCoalesceKey>,
            ),
        >,
    >,
>;

/// 决议一笔审批：唤醒该 request_id 的等待者，并把同合并键的并发等待者一并
/// 唤醒（Op::ApprovalReply 的处理路径）。UI 审批条同时只能显示一笔，Swarm
/// 多个子代理同命令并发等审批时，不扇出的话被顶掉的等待者永远无人应答。
pub fn resolve_approval(pending: &PendingApprovals, request_id: &str, decision: ApprovalDecision) {
    let mut pending = pending.lock().expect("pending lock");
    let coalesce = pending.get(request_id).and_then(|(_, key)| key.clone());
    if let Some((reply, _)) = pending.remove(request_id) {
        let _ = reply.send(decision);
    }
    let Some(key) = coalesce else { return };
    let same_key: Vec<String> = pending
        .iter()
        .filter(|(_, (_, other))| other.as_ref() == Some(&key))
        .map(|(id, _)| id.clone())
        .collect();
    for id in same_key {
        if let Some((reply, _)) = pending.remove(&id) {
            let _ = reply.send(decision);
        }
    }
}

/// 等待中的结构化提问：request_id → 回执通道。None = 用户跳过；
/// 外层按题、内层为该题选中标签（"其他"自由文本作为标签原样放入）。
pub type PendingQuestions = Arc<Mutex<HashMap<String, oneshot::Sender<Option<Vec<Vec<String>>>>>>>;

/// 会话级模型覆盖（SetModel）
#[derive(Clone, Debug, Default)]
pub struct ModelSelection {
    pub provider_id: String,
    pub model_id: String,
    pub reasoning_level: Option<String>,
}

/// SessionMeta 里持久化的模型选择 → ModelSelection（缺 provider/model 则无覆盖）
fn meta_to_selection(meta: &SessionMeta) -> Option<ModelSelection> {
    match (&meta.provider_id, &meta.model_id) {
        (Some(provider_id), Some(model_id)) => Some(ModelSelection {
            provider_id: provider_id.clone(),
            model_id: model_id.clone(),
            reasoning_level: meta.reasoning_level.clone(),
        }),
        _ => None,
    }
}

/// provider+model(+推理等级) → 端点解析。
/// `default_level`：无模型覆盖时使用的思考等级（作用于配置默认模型）。
pub fn resolve_model(
    config: &AppConfig,
    selection: Option<&ModelSelection>,
    default_level: Option<&str>,
) -> Option<ResolvedModel> {
    let (provider_id, model_id, level) = match selection {
        Some(sel) => (
            sel.provider_id.clone(),
            sel.model_id.clone(),
            sel.reasoning_level.clone(),
        ),
        None => (
            config.default_provider.clone(),
            config.default_model.clone(),
            default_level.map(str::to_string),
        ),
    };
    let provider = config
        .providers
        .iter()
        .find(|p| p.enabled && p.id == provider_id)
        .or_else(|| config.providers.iter().find(|p| p.enabled))?;
    let model = provider
        .models
        .iter()
        .find(|m| m.id == model_id)
        .or_else(|| provider.models.first())?;
    let reasoning_params = level
        .as_ref()
        .and_then(|level| model.reasoning_params.get(level).cloned());
    Some(ResolvedModel {
        base_url: provider.base_url.clone(),
        api_key: config::expand_env(&provider.api_key),
        model: model.id.clone(),
        context_window: model.context_window,
        max_output_tokens: model.max_output_tokens,
        api_format: provider.api_format,
        reasoning_params,
        cap_web_search: model.cap_web_search,
        web_search_tool: model.web_search_tool.clone(),
        input_image: model.input_image,
        provider_name: provider.name.clone(),
    })
}

pub struct Session {
    pub id: String,
    pub cwd: PathBuf,
    history: Vec<ChatMsg>,
    /// 上一条调用轨迹的完整输入投影（增量落盘的前缀基准；resume 后为空，
    /// 首条自动退回全量，自愈）
    io_last_input: Vec<crate::model_io::ModelIoMessage>,
    /// 事件序号（Arc 共享：后台子代理任务经 emit_bg 用同一计数器发事件）
    seq: Arc<std::sync::atomic::AtomicU64>,
    turn_counter: u64,
    tracker: ChangeTracker,
    state: crate::task::SessionToolState,
    /// 「本会话内始终允许」的记忆：(工具名, subject)——Bash=命令首词，Write/Edit=路径
    always_allowed: HashSet<(String, String)>,
    /// 项目级 allow/deny 规则（.pigcode/permissions.toml，会话创建/回放时加载一次）
    permissions: crate::permissions::PermissionRules,
    /// EnterPlanMode 进入计划模式前的模式（ExitPlanMode 确认后恢复；内存态不持久化）
    pre_plan_mode: Option<ExecMode>,
    pending: PendingApprovals,
    pending_questions: PendingQuestions,
    mode: ExecMode,
    model_override: Option<ModelSelection>,
    rollout: Option<Rollout>,
    store: Arc<Mutex<Store>>,
    data_dir: PathBuf,
    /// 会话开始时的 git 快照（分支+dirty），env 块复用——每回合实时查询会让
    /// 系统提示词前缀缓存随第一次编辑/提交来回翻转失效
    git_snapshot: Option<String>,
    /// 会话开始时冻结的 AGENTS.md 段（系统提示词注入用）。中途变更经
    /// turn_reminder 推送新内容，冻结版不回写——保前缀缓存（kimi
    /// agentsMdReminder 同款取舍）
    agents_prompt: String,
    /// 会话开始时冻结的技能清单段（系统提示词注入用；正文仍由 Skill 工具
    /// 按需现读）。每回合重扫会让设置页增删改技能打断前缀缓存（kimi-code
    /// frozenSkillListing 同款取舍），代价是改动只对新会话生效
    skills_prompt: String,
    /// 会话开始时冻结的日期（env 块展示用）；跨天经 turn_reminder 更正
    date_frozen: String,
    /// 上次已提醒的日期/AGENTS.md 内容（turn_reminder 去重：与冻结值不同
    /// 才提醒，同内容不重复注入）
    date_reminded: String,
    agents_reminded: String,
    /// 会话开始时冻结的子代理档案快照（Agent/AgentSwarm 工具 description
    /// 内嵌档案清单用——每步重扫会让编辑档案打断 tools 前缀缓存；spawn
    /// 执行时另走 load_profiles 现读，清单过期由报错自愈）
    profiles_snapshot: Vec<crate::agent::AgentProfile>,
    last_total_tokens: Option<u64>,
    /// 当前回合累计的 token 用量（回合结束写入 turn_usage 表）
    turn_input: u64,
    turn_cache_read: u64,
    turn_output: u64,
    /// 当前回合累计的纯 API 耗时（只算 provider 请求，不含工具执行/审批等待；
    /// token 速度用它算，避免跑长命令把速度拉低）
    turn_api_ms: u64,
    /// turn_api_ms 中等待首个输出 token 的时间（首字时间；多步调用累计）
    turn_ttft_ms: u64,
    /// 回合内 provider 请求次数（平均首字 = turn_ttft_ms / turn_api_steps）
    turn_api_steps: u64,
    /// 会话累计（回放恢复）：未命中输入 / 命中缓存输入，平均缓存命中率用
    input_total: u64,
    cache_read_total: u64,
    /// 子代理序号（agent_id = a{时间戳}-{agent_seq+1}；带时间戳，跨重启不撞已持久化的子代理文件）
    agent_seq: u64,
    /// 应用配置快照（子代理显式模型解析用；None = 未加载，仅继承可用）
    app_config: Option<AppConfig>,
    /// MCP 连接管理器：首个 step 采样前懒连接（None = 尚未尝试）；Arc 共享——
    /// 并发组 spawn 任务与子代理后台任务 clone  owned 句柄现取工具；
    /// 子进程 kill_on_drop 兜底，会话析构即回收
    mcp: Option<Arc<crate::mcp::McpManager>>,
}

enum StepOutcome {
    TextOnly,
    ToolsExecuted,
    Ended,
}

/// 自由路径（后台子代理任务/门控自由函数）的事件发送：seq 原子递增，
/// 与 Session::emit 共享同一 Arc 计数器（前台/后台事件 seq 不错乱）。
pub(crate) fn emit_bg(
    session_id: &str,
    seq: &std::sync::atomic::AtomicU64,
    tx: &async_channel::Sender<Event>,
    build: impl FnOnce(String, u64) -> Event,
) {
    let next = seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    let _ = tx.send_blocking(build(session_id.to_string(), next));
}

/// 门控执行的上下文（exec_tool_gated_ctx 的入参包）：Session 薄封装按字段借用组装，
/// 后台子代理任务按 owned 快照/Arc 克隆组装——同一门控前台/后台都能跑。
pub(crate) struct GateCtx<'a> {
    pub cwd: &'a Path,
    pub mode: ExecMode,
    pub tracker: &'a mut ChangeTracker,
    pub state: &'a crate::task::SessionToolState,
    pub pending: &'a PendingApprovals,
    pub permissions: &'a crate::permissions::PermissionRules,
    pub always_allowed: &'a mut HashSet<(String, String)>,
    pub session_id: &'a str,
    pub seq: &'a std::sync::atomic::AtomicU64,
    pub store: &'a Arc<Mutex<Store>>,
    /// 内置工具以外的运行时工具（MCP）：执行段的按名查找兜底；
    /// 根会话传全部已连接 MCP 工具，子代理传按档案继承规则收窄后的子集
    pub extra_tools: &'a [Box<dyn tool::Tool>],
}

/// 门控执行的自由函数实现（Session::exec_tool_gated 与后台子代理驱动共用）：
/// 危险黑名单 → permissions deny/allow → AutoEdit 只读直通 → 审批门 → 执行 →
/// 会话级副作用（TodoList 快照落库+推送、FileChanged 落库+推送、file_originals 落库）。
/// ToolCallBegin/history.push/RolloutRecord::ToolCall/ToolCallEnd 不在此——
/// 由调用方负责（父/子各写各的历史与 rollout）。
/// `tool` 为 None（未知工具名）时审批/权限按名匹配全部落空，直达执行段报「未知工具」。
pub(crate) async fn exec_tool_gated_ctx(
    ctx: &mut GateCtx<'_>,
    call: &ToolCall,
    tool: Option<&dyn tool::Tool>,
    item_id: &str,
    turn_id: &str,
    tx: &async_channel::Sender<Event>,
    cancel: &CancellationToken,
) -> GatedToolOutcome {
    // 黑名单命中的危险命令强制弹窗（ZCode alwaysAsk 同款）：Yolo 之外的模式都弹，
    // always_allowed 对其不生效；Plan 模式已在调用方整类硬拒，不走这里。
    // Yolo（容器/沙箱无管制）连危险判定都跳过，什么弹窗都不发。
    let bash_command = if call.name == "Bash" {
        serde_json::from_str::<serde_json::Value>(&call.arguments)
            .ok()
            .and_then(|args| args["command"].as_str().map(str::to_string))
    } else {
        None
    };

    // 项目规则（.pigcode/permissions.toml）：deny 命中 → 所有模式（含 Yolo）
    // 硬拒，排在危险弹窗之前（用户显式写的 deny 是最强意图）。
    // 注意 Bash 匹配完整命令串（比 always_allowed 的首词粒度更精细）。
    let perm_subject = match call.name.as_str() {
        "Bash" => bash_command.clone(),
        "Write" | "Edit" => Some(tool::approval_subject(call)),
        // MCP 工具以全名为 subject：项目规则 `mcp__fs__write(*)` 可命中
        name if name.starts_with("mcp__") => Some(call.name.clone()),
        _ => None,
    };
    if let Some(subject) = perm_subject.as_deref()
        && let Some(rule) = ctx.permissions.deny_hit(&call.name, subject)
    {
        let note = format!("项目规则禁止执行: {rule}（.pigcode/permissions.toml）");
        return GatedToolOutcome::Rejected { note };
    }
    // allow 命中免审批（危险命令除外——危险判定在下方弹窗优先）
    let allowed_by_rules = perm_subject
        .as_deref()
        .is_some_and(|subject| ctx.permissions.allow_hit(&call.name, subject));

    let danger_reason = if ctx.mode == ExecMode::Yolo {
        None
    } else {
        bash_command.as_deref().and_then(tool::is_dangerous_command)
    };
    // AutoEdit 直通保守白名单的只读命令（ls/git status 这类）；危险判定在上方优先。
    // 吐文件类命令（cat 等）在此做参数级判定：敏感/越界目标不放行
    let readonly_bash = ctx.mode == ExecMode::AutoEdit
        && bash_command
            .as_deref()
            .is_some_and(|command| tool::is_readonly_command(command, ctx.cwd));
    // 「本会话内始终允许」细化到 (工具, subject)：Bash=命令首词，Write/Edit=路径
    let approval_key = (call.name.clone(), tool::approval_subject(call));

    if danger_reason.is_some()
        || (tool.is_some_and(|t| tool::requires_approval(t, ctx.mode))
            && !ctx.always_allowed.contains(&approval_key)
            && !readonly_bash
            && !allowed_by_rules)
    {
        let request_id = format!("{}-{turn_id}-approval-{item_id}", ctx.session_id);
        let detail_text = approval_detail(call, ctx.cwd, danger_reason);
        // 合并键复用 perm_subject（Bash=完整命令，Write/Edit=路径，MCP=工具名）；
        // 无 subject 的工具不合并（保守），危险位入键——普通弹窗不串到危险弹窗
        let coalesce_key = perm_subject
            .clone()
            .map(|subject| (call.name.clone(), subject, danger_reason.is_some()));
        let (reply_tx, reply_rx) = oneshot::channel();
        ctx.pending
            .lock()
            .expect("pending lock")
            .insert(request_id.clone(), (reply_tx, coalesce_key));
        emit_bg(ctx.session_id, ctx.seq, tx, |session_id, seq| {
            Event::ApprovalRequested {
                session_id,
                seq,
                request_id: request_id.clone(),
                tool: call.name.clone(),
                detail: detail_text,
            }
        });
        let decision = tokio::select! {
            reply = reply_rx => reply.unwrap_or(ApprovalDecision::Reject),
            _ = cancel.cancelled() => {
                ctx.pending.lock().expect("pending lock").remove(&request_id);
                return GatedToolOutcome::Cancelled;
            }
        };
        match decision {
            ApprovalDecision::Allow => {}
            ApprovalDecision::AlwaysAllow => {
                // 危险命令不记入 always_allowed：只在本次放行，等价 Allow
                if danger_reason.is_none() {
                    ctx.always_allowed.insert(approval_key);
                }
            }
            ApprovalDecision::Reject => {
                let note = match danger_reason {
                    Some(reason) => format!(
                        "用户拒绝了该高风险命令（{reason}）。请尊重用户意愿，改用其他方式或说明理由后继续。"
                    ),
                    None => "用户拒绝了该操作。请尊重用户意愿，改用其他方式或说明理由后继续。"
                        .to_string(),
                };
                return GatedToolOutcome::Rejected { note };
            }
        }
    }

    let result = {
        let tool_ctx = ToolContext {
            cwd: ctx.cwd,
            tracker: ctx.tracker,
            state: ctx.state,
        };
        tokio::select! {
            result = tool::execute_with_extra(call, tool_ctx, ctx.extra_tools) => Some(result),
            _ = cancel.cancelled() => None,
        }
    };
    let Some((output, is_error, file_change, edit, images)) = result else {
        return GatedToolOutcome::Cancelled;
    };
    // 图片随 history 进模型上下文（Anthropic blocks / OpenAI 拆 user 消息）；
    // rollout 的 ToolCall 记录只存 output 文本（尺寸摘要在内），base64 不落盘
    let chat_images = tool_images_to_chat(&call.arguments, images);
    // TodoList 写入成功后向 UI 推待办快照（读操作输出即列表，无需重复推）
    if call.name == "TodoList" && !is_error {
        let items = ctx.state.todos.lock().expect("todos lock").clone();
        // 写操作（带 todos 参数）落 SQLite todos 表（当前态 upsert）；
        // 读操作不落盘。事件流 JSONL 不再记状态快照
        let is_write = serde_json::from_str::<serde_json::Value>(&call.arguments)
            .ok()
            .is_some_and(|v| v.get("todos").is_some());
        if is_write {
            let json = serde_json::to_string(&items).unwrap_or_default();
            ctx.store
                .lock()
                .expect("store lock")
                .set_todos(ctx.session_id, &json);
        }
        emit_bg(ctx.session_id, ctx.seq, tx, |session_id, seq| {
            Event::TodoListChanged {
                session_id,
                seq,
                items,
            }
        });
    }
    if let Some(change) = file_change {
        // 改动当前态落 SQLite file_changes 表（按路径 upsert；净额归零删行），
        // JSONL 只留消息/工具事件流
        {
            let store = ctx.store.lock().expect("store lock");
            if change.additions == 0 && change.deletions == 0 {
                store.delete_file_change(ctx.session_id, &change.path);
            } else {
                store.upsert_file_change(
                    ctx.session_id,
                    &change.path,
                    &change.unified_diff,
                    change.additions,
                    change.deletions,
                );
            }
        }
        emit_bg(ctx.session_id, ctx.seq, tx, |session_id, seq| {
            Event::FileChanged {
                session_id,
                seq,
                path: change.path,
                unified_diff: change.unified_diff,
                additions: change.additions,
                deletions: change.deletions,
            }
        });
    }
    // 本工具新增的原始快照落 file_originals 表（跨重启 diff 基线 / revert）；
    // 超过 4MB 的大文件不持久化（基线退回进程内存，与 kimi-code 口径一致）
    let dirty = ctx.tracker.take_dirty();
    if !dirty.is_empty() {
        const MAX_ORIGINAL_BYTES: usize = 4 * 1024 * 1024;
        let store = ctx.store.lock().expect("store lock");
        for path in dirty {
            if let Some(original) = ctx.tracker.original(&path) {
                let oversized = original
                    .as_ref()
                    .is_some_and(|content| content.len() > MAX_ORIGINAL_BYTES);
                if !oversized {
                    store.upsert_file_original(
                        ctx.session_id,
                        &path.to_string_lossy(),
                        original.as_deref(),
                    );
                }
            }
        }
    }
    GatedToolOutcome::Executed {
        output,
        is_error,
        edit,
        images: chat_images,
    }
}

/// ToolImage → ChatImage：label 取调用参数里的 path（门控路径与并发只读段共用）。
fn tool_images_to_chat(
    arguments: &str,
    images: Vec<tool::ToolImage>,
) -> Vec<crate::provider::ChatImage> {
    let label = serde_json::from_str::<serde_json::Value>(arguments)
        .ok()
        .and_then(|v| v["path"].as_str().map(str::to_string));
    images
        .into_iter()
        .map(|img| crate::provider::ChatImage {
            media_type: img.media_type,
            data_base64: img.data_base64,
            label: label.clone(),
        })
        .collect()
}

/// 门控工具执行（exec_tool_gated）的结果
pub(crate) enum GatedToolOutcome {
    /// 已执行；调用方负责 history/rollout/ToolCallEnd（父/子各写各的）
    Executed {
        output: String,
        is_error: bool,
        /// 本次编辑的 diff（父会话工具卡片内联渲染用；子代理路径忽略）
        edit: Option<pig_protocol::EditDiff>,
        images: Vec<crate::provider::ChatImage>,
    },
    /// 审批/规则拒绝（note 即给模型的文案）；调用方负责 history/rollout/ToolCallEnd
    Rejected { note: String },
    /// 取消（审批等待或执行中被打断）；调用方决定 TurnAborted 与收尾
    Cancelled,
}

/// 子代理委派（run_subagent/run_swarm）的结果
pub(crate) enum SubagentOutcome {
    /// 子代理已收尾（成败都在 note 里；后台派发则是即时回执）；调用方负责
    /// history/rollout/ToolCallEnd。card = 单代理卡元信息（Agent 路径；随 rollout
    /// ToolCall 记录持久化，回放重建代理卡）；cards = 批量代理卡（AgentSwarm 路径，
    /// 每个子代理一张）。参数/档案/模型解析失败的早退没有 agent_id，两者皆空
    Finished {
        note: String,
        is_error: bool,
        card: Option<crate::rollout::AgentCardRecord>,
        cards: Vec<crate::rollout::AgentCardRecord>,
    },
    /// 取消（审批等待或采样/执行中被打断）；调用方发 TurnAborted 并收尾。
    /// card/cards = 代理卡元信息（取消同样随 rollout 持久化，回放重建代理卡）
    Cancelled {
        card: Option<crate::rollout::AgentCardRecord>,
        cards: Vec<crate::rollout::AgentCardRecord>,
    },
}

/// settle_cancelled_tool 的入参组：当前被取消的调用 + 同响应剩余未执行的调用。
struct CancelledTool<'a> {
    call: &'a crate::provider::ToolCall,
    /// 工具卡摘要（随 rollout 记录持久化）
    summary: String,
    item_id: &'a str,
    /// 同响应里排在当前之后的调用（不会再执行，补空回执保持 tool_use 配对）
    rest: &'a [crate::provider::ToolCall],
    /// 代理卡元信息（Agent 工具取消路径；其余工具 None）
    card: Option<crate::rollout::AgentCardRecord>,
    /// 批量代理卡元信息（AgentSwarm 工具取消路径；其余工具空列表）
    cards: Vec<crate::rollout::AgentCardRecord>,
}

impl Session {
    #[allow(clippy::too_many_arguments)]
    pub fn create(
        meta: SessionMeta,
        pending: PendingApprovals,
        pending_questions: PendingQuestions,
        store: Arc<Mutex<Store>>,
        sessions_dir: &Path,
        data_dir: PathBuf,
        task_notify: tokio::sync::mpsc::UnboundedSender<String>,
        wake_notify: tokio::sync::mpsc::UnboundedSender<(String, String)>,
        app_config: Option<&AppConfig>,
    ) -> Result<Self, String> {
        let rollout = Rollout::create(sessions_dir, &meta)?;
        // 字面量里 data_dir 会被 move，冻结快照先算局部（创建时刻目录现状）
        let skills_prompt = crate::skills::skills_section(&meta.cwd, &data_dir);
        let agents_prompt = prompt::agents_md(&data_dir, &meta.cwd);
        let profiles_snapshot = crate::agent::load_profiles(&meta.cwd, &data_dir);
        let today = prompt::today();
        Ok(Self {
            id: meta.id.clone(),
            cwd: meta.cwd.clone(),
            history: Vec::new(),
            io_last_input: Vec::new(),
            seq: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            turn_counter: 0,
            tracker: ChangeTracker::default(),
            state: crate::task::SessionToolState::new(
                meta.id,
                task_notify,
                wake_notify,
                // sessions 子树（子代理 result.md/上下文 jsonl）始终可读
                vec![data_dir.join("sessions")],
            ),
            always_allowed: HashSet::new(),
            permissions: load_permissions(&meta.cwd),
            pre_plan_mode: None,
            pending,
            pending_questions,
            mode: ExecMode::ConfirmBeforeEdit,
            model_override: None,
            rollout: Some(rollout),
            store,
            data_dir,
            git_snapshot: prompt::git_snapshot(&meta.cwd),
            agents_prompt: agents_prompt.clone(),
            skills_prompt,
            date_frozen: today.clone(),
            date_reminded: today,
            agents_reminded: agents_prompt,
            profiles_snapshot,
            last_total_tokens: None,
            turn_input: 0,
            turn_cache_read: 0,
            turn_output: 0,
            turn_api_ms: 0,
            turn_ttft_ms: 0,
            turn_api_steps: 0,
            input_total: 0,
            cache_read_total: 0,
            agent_seq: 0,
            app_config: app_config.cloned(),
            mcp: None,
        })
    }

    /// 从 rollout 重建。diff 基线从 file_originals 表恢复到 ChangeTracker：
    /// resume 后改动仍以「会话首次快照 → 当前」计算，revert 跨重启可用。
    #[allow(clippy::too_many_arguments)]
    pub fn load(
        id: &str,
        sessions_dir: &Path,
        pending: PendingApprovals,
        pending_questions: PendingQuestions,
        store: Arc<Mutex<Store>>,
        data_dir: PathBuf,
        task_notify: tokio::sync::mpsc::UnboundedSender<String>,
        wake_notify: tokio::sync::mpsc::UnboundedSender<(String, String)>,
        app_config: Option<&AppConfig>,
    ) -> Result<(Self, Vec<RolloutRecord>), String> {
        let records = Rollout::load(&sessions_dir.join(format!("{id}.jsonl")))?;
        let Some(RolloutRecord::Meta { cwd, .. }) = records.first() else {
            return Err(format!("rollout 缺少 meta 行: {id}"));
        };
        // 恢复会话重新冻结技能清单/AGENTS.md/日期/子代理档案（以恢复时刻目录现状为准）
        let skills_prompt = crate::skills::skills_section(cwd, &data_dir);
        let agents_prompt = prompt::agents_md(&data_dir, cwd);
        let profiles_snapshot = crate::agent::load_profiles(cwd, &data_dir);
        let today = prompt::today();
        let history = rebuild_history(
            &records,
            // 占位系统提示词：首轮 run_turn 会用冻结快照整体覆盖
            prompt::system_prompt(cwd, true, None, &today, &agents_prompt, &skills_prompt),
        );
        let originals = store
            .lock()
            .expect("store lock")
            .file_originals(id)
            .into_iter()
            .map(|(path, content)| (PathBuf::from(path), content))
            .collect();
        let mut tracker = ChangeTracker::default();
        tracker.restore(originals);
        // resume 后接管同一 JSONL 继续追加；失败要响亮（置 None 会静默丢后续所有记录）
        let rollout = Rollout::open_append(sessions_dir, id)?;
        let session = Self {
            id: id.to_string(),
            cwd: cwd.clone(),
            history,
            io_last_input: Vec::new(),
            seq: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            turn_counter: 0,
            tracker,
            state: crate::task::SessionToolState::new(
                id.to_string(),
                task_notify,
                wake_notify,
                // sessions 子树（子代理 result.md/上下文 jsonl）始终可读
                vec![data_dir.join("sessions")],
            ),
            always_allowed: HashSet::new(),
            permissions: load_permissions(cwd),
            pre_plan_mode: None,
            pending,
            pending_questions,
            mode: ExecMode::ConfirmBeforeEdit,
            model_override: None,
            rollout: Some(rollout),
            store,
            data_dir,
            git_snapshot: prompt::git_snapshot(cwd),
            agents_prompt: agents_prompt.clone(),
            skills_prompt,
            date_frozen: today.clone(),
            date_reminded: today,
            agents_reminded: agents_prompt,
            profiles_snapshot,
            last_total_tokens: None,
            turn_input: 0,
            turn_cache_read: 0,
            turn_output: 0,
            turn_api_ms: 0,
            turn_ttft_ms: 0,
            turn_api_steps: 0,
            input_total: 0,
            cache_read_total: 0,
            agent_seq: 0,
            app_config: app_config.cloned(),
            mcp: None,
        };
        Ok((session, records))
    }

    /// 根会话工具集：内置 + Agent/AgentSwarm + MCP。子代理循环用 tool::all()
    /// 收窄 + MCP 继承规则，天然无 Agent/AgentSwarm 防嵌套；档案用会话冻结
    /// 快照、MCP 清单随懒连接快照——tools 在缓存前缀最前面，会话内字节稳定
    pub(crate) fn root_tools(&self) -> Vec<Box<dyn tool::Tool>> {
        let mut tools = tool::all_root(&self.cwd, &self.data_dir, &self.profiles_snapshot);
        if let Some(mcp) = &self.mcp {
            tools.extend(mcp.tools());
        }
        tools
    }

    /// 根会话工具 schema 集（run_step 采样用）
    pub(crate) fn root_schemas(&self) -> Vec<serde_json::Value> {
        self.root_tools().iter().map(|tool| tool.schema()).collect()
    }

    fn emit(
        &mut self,
        build: impl FnOnce(String, u64) -> Event,
        tx: &async_channel::Sender<Event>,
    ) {
        emit_bg(&self.id, &self.seq, tx, build);
    }

    fn record(&mut self, record: &RolloutRecord) {
        if let Some(rollout) = &mut self.rollout {
            rollout.append(record);
        }
    }

    /// 工具执行中取消（用户点停止）的统一收尾：当前调用补「已停止」回执——
    /// 历史（tool_use/tool_result 配对，防下次请求悬空被 API 拒）、rollout
    /// （重启回放重建卡片，不凭空消失）、ToolCallEnd（落定 live 卡片）；
    /// 同响应里排在后面的调用不会再执行，一并补回执保持配对完整。
    fn settle_cancelled_tool(
        &mut self,
        cancelled: CancelledTool<'_>,
        tx: &async_channel::Sender<Event>,
    ) {
        self.history.push(ChatMsg::tool_result(
            &cancelled.call.id,
            "已停止".to_string(),
        ));
        for rest in cancelled.rest {
            self.history
                .push(ChatMsg::tool_result(&rest.id, "已停止".to_string()));
        }
        self.record(&RolloutRecord::ToolCall {
            tool: cancelled.call.name.clone(),
            summary: cancelled.summary,
            arguments: cancelled.call.arguments.clone(),
            output: "已停止".to_string(),
            is_error: false,
            edit: None,
            agent_card: cancelled.card,
            agent_cards: cancelled.cards,
        });
        let item_id = cancelled.item_id.to_string();
        self.emit(
            |session_id, seq| Event::ToolCallEnd {
                session_id,
                seq,
                item_id,
                output: "已停止".to_string(),
                is_error: false,
                edit: None,
            },
            tx,
        );
    }

    /// 回合收尾：产出「本轮改动」（落 rollout + 推 UI 消息流面板）；无改动不发。
    fn flush_turn_changes(&mut self, tx: &async_channel::Sender<Event>) {
        let changes = self.tracker.take_turn_changes(&self.cwd);
        if changes.is_empty() {
            return;
        }
        let files: Vec<pig_protocol::EditDiff> = changes.into_iter().map(Into::into).collect();
        self.record(&RolloutRecord::TurnChanges {
            files: files.clone(),
        });
        self.emit(
            |session_id, seq| Event::TurnFileChanges {
                session_id,
                seq,
                files,
            },
            tx,
        );
    }

    fn touch_index(&mut self) {
        let id = self.id.clone();
        self.store
            .lock()
            .expect("store lock")
            .update_session(&id, |meta| meta.updated_at = now_secs());
    }

    pub fn set_mode(&mut self, mode: ExecMode) {
        self.mode = mode;
    }

    /// 会话级「工作区外读/写」开关（写进共享 state，回合进行中也生效）
    pub fn set_fs_access(&mut self, read_outside: bool, write_outside: bool) {
        use std::sync::atomic::Ordering;
        self.state
            .fs_read_outside
            .store(read_outside, Ordering::Relaxed);
        self.state
            .fs_write_outside
            .store(write_outside, Ordering::Relaxed);
    }

    pub fn set_model(&mut self, selection: ModelSelection) {
        self.model_override = Some(selection);
    }

    pub fn revert_file(&mut self, path: &str, tx: &async_channel::Sender<Event>) {
        let result = tool::resolve_checked(&self.cwd, path, false)
            .and_then(|full| self.tracker.revert(&full).map(|()| full));
        match result {
            Ok(full) => {
                // 撤销后改动与基线都不再有意义：清库（改动行 + 原始快照行）
                {
                    let store = self.store.lock().expect("store lock");
                    store.delete_file_change(&self.id, path);
                    store.delete_file_original(&self.id, &full.to_string_lossy());
                }
                self.emit(
                    |session_id, seq| Event::FileReverted {
                        session_id,
                        seq,
                        path: path.to_string(),
                    },
                    tx,
                );
            }
            Err(error) => self.emit(
                |session_id, seq| Event::Error {
                    session_id: Some(session_id),
                    seq,
                    message: error,
                },
                tx,
            ),
        }
    }
}

mod approval;
mod compact;
mod images;
mod replay;
mod runner;
mod subagent;
mod title;
mod turn;

// 大 impl 拆到子模块(同 crate 内 impl 块可分散;子模块可见根的私有项),
// 对外 API 由显式 re-export 钉住;跨子模块引用经根转发。
pub use approval::approval_detail;
pub(crate) use approval::exec_mode_label;
pub use compact::COMPACTION_MARKER;
pub(crate) use images::compression_note;
pub use images::project_images;
pub use runner::agent_loop;
pub use title::TITLE_PROMPT_MARKER;
pub(crate) use title::spawn_title_generation;

/// 加载项目级权限规则：文件缺失 = 空规则；解析失败不致命，stderr 提示（
/// 会话创建/回放路径没有合适的事件通道，不硬建）
fn load_permissions(cwd: &std::path::Path) -> crate::permissions::PermissionRules {
    match crate::permissions::PermissionRules::load(cwd) {
        Ok(rules) => {
            if rules.skipped > 0 {
                eprintln!(
                    "[permissions] {} 条规则语法错误已跳过（.pigcode/permissions.toml）",
                    rules.skipped
                );
            }
            rules
        }
        Err(error) => {
            eprintln!("[permissions] 加载失败（按空规则继续）: {error}");
            crate::permissions::PermissionRules::default()
        }
    }
}
