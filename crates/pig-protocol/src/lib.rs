//! pig-code 的 UI 与 core 之间的契约类型（见 docs/PLAN.md §2.2）。
//! 纯 serde 类型，无业务逻辑；UI 与 core 双向依赖本 crate。
//!
//! 原则：delta 事件（流式碎片，live-only）与 done 事件（全量终值，可回放）分离；
//! 会话事件带 session_id + seq（per-session 单调递增）。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// UI → core 命令
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Op {
    NewSession {
        cwd: PathBuf,
        /// UI 当前模型选择；None → 工作区最近活跃会话 / 配置默认
        provider_id: Option<String>,
        model_id: Option<String>,
        /// UI 当前思考等级（原样采用，None = 关；种子的等级经 UI hero 默认值下达）
        reasoning_level: Option<String>,
        /// UI 当前执行模式；None → 工作区最近活跃会话 / 默认
        exec_mode: Option<ExecMode>,
    },
    OpenSession {
        session_id: String,
    },
    ListSessions,
    ListWorkspaces,
    AddWorkspace {
        path: PathBuf,
    },
    /// 从侧栏移除工作区：置为隐藏，条目与会话数据保留；
    /// 该工作区下新建会话时自动恢复
    RemoveWorkspace {
        path: PathBuf,
    },
    /// 重命名工作区显示名；alias 为 None 表示恢复默认目录名
    RenameWorkspace {
        path: PathBuf,
        alias: Option<String>,
    },
    UpdateSessionMeta {
        session_id: String,
        pinned: Option<bool>,
        archived: Option<bool>,
        /// 手动重命名（置 title_custom，此后自动命名不再覆盖）
        title: Option<String>,
    },
    /// 删除会话：清 sessions 及关联表 + rollout JSONL，不可恢复
    DeleteSession {
        session_id: String,
    },
    SendMessage {
        session_id: String,
        content: String,
        files: Vec<String>,
        mode: ExecMode,
    },
    Interrupt {
        session_id: String,
    },
    ApprovalReply {
        request_id: String,
        decision: ApprovalDecision,
    },
    /// 结构化提问的回复：None = 用户跳过；外层按题、内层为该题选中标签
    ///（"其他"自由文本作为标签原样放入）
    QuestionReply {
        request_id: String,
        answers: Option<Vec<Vec<String>>>,
    },
    SetModel {
        session_id: String,
        provider_id: String,
        model_id: String,
        /// None = 不启用推理参数
        reasoning_level: Option<String>,
    },
    /// 单独设置思考等级：无模型覆盖时同样生效（作用于配置默认模型）并持久化
    SetReasoning {
        session_id: String,
        reasoning_level: Option<String>,
    },
    GetConfig,
    SaveConfig {
        config: AppConfig,
    },
    TestProvider {
        provider_id: String,
    },
    SetExecMode {
        session_id: String,
        mode: ExecMode,
    },
    /// 会话级「工作区外读/写」开关（默认关；tmp 目录始终放行）
    SetFsAccess {
        session_id: String,
        read_outside: bool,
        write_outside: bool,
    },
    RevertFile {
        session_id: String,
        path: String,
    },
    SearchFiles {
        session_id: String,
        query: String,
    },
    /// 非会话态：查询目录的 git 信息（hero 分支选择器）
    GitInfo {
        cwd: PathBuf,
    },
    CheckoutBranch {
        cwd: PathBuf,
        branch: String,
    },
    /// 非会话态：工作区 git 改动列表（未暂存 + 已暂存，含 untracked 行数统计）
    GitStatus {
        cwd: PathBuf,
    },
    /// 非会话态：单文件 git diff 原文（staged=false 未暂存 / true 已暂存）
    GitDiff {
        cwd: PathBuf,
        path: String,
        staged: bool,
    },
    /// 取消该会话队首的排队消息（FIFO）
    CancelQueued {
        session_id: String,
        text: String,
    },
    /// 非会话态：按模型 ID 查 models.dev 元数据（上下文/输入输出上限/推理等级）；
    /// 磁盘缓存命中直接回，未命中（新模型）重新拉取后再回
    ModelLookup {
        id: String,
    },
    Compact {
        session_id: String,
    },
    Shutdown,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecMode {
    #[default]
    ConfirmBeforeEdit,
    AutoEdit,
    Plan,
    FullAccess,
    /// 无管制全自动（容器/沙箱场景）：无审批，危险命令也不拦截
    Yolo,
}

/// 单次编辑（Write/Edit）产生的文件 diff：UI 工具卡片内联渲染用。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EditDiff {
    pub path: String,
    pub unified_diff: String,
    pub additions: u32,
    pub deletions: u32,
}

/// git 工作区单文件改动（未暂存/已暂存列表项）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GitFileChange {
    pub path: String,
    pub additions: u32,
    pub deletions: u32,
    /// porcelain 状态：M/A/D/R/C（冲突）/?（未跟踪）
    pub status: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApprovalDecision {
    Allow,
    AlwaysAllow,
    Reject,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApiFormat {
    OpenAiChat,
    AnthropicMessages,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelConfig {
    pub id: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub context_window: u64,
    pub max_output_tokens: u64,
    // 输入类型（文本恒有）
    #[serde(default)]
    pub input_image: bool,
    #[serde(default)]
    pub input_video: bool,
    #[serde(default)]
    pub input_pdf: bool,
    // 能力标记（存储为主，暂不全部消费）
    #[serde(default)]
    pub cap_structured: bool,
    #[serde(default)]
    pub cap_web_search: bool,
    /// 原生联网搜索工具定义：Anthropic 缺省 web_search_20250305；OpenAI 兼容
    /// 端点需显式配置（如智谱 {"type":"web_search","web_search":{...}}）
    #[serde(default)]
    pub web_search_tool: Option<serde_json::Value>,
    #[serde(default)]
    pub cap_system_msg: bool,
    /// 可选推理等级，如 ["low","high","max"]
    #[serde(default)]
    pub reasoning_levels: Vec<String>,
    /// 默认思考等级：新会话未指定等级、切换模型等级不适配时的初始档；
    /// None = 未设置（沿用现状：启发式兜底/关）
    #[serde(default)]
    pub default_reasoning_level: Option<String>,
    /// 等级 id → 界面显示名（如 max → "最高"）；纯展示层，请求仍按 id 合并参数
    #[serde(default)]
    pub reasoning_labels: std::collections::HashMap<String, String>,
    /// 等级 → 合并进请求体的 JSON
    #[serde(default)]
    pub reasoning_params: std::collections::HashMap<String, serde_json::Value>,
}

fn default_true() -> bool {
    true
}

impl ModelConfig {
    pub fn new(id: &str, context_window: u64, max_output_tokens: u64) -> Self {
        Self {
            id: id.to_string(),
            enabled: true,
            context_window,
            max_output_tokens,
            input_image: false,
            input_video: false,
            input_pdf: false,
            cap_structured: false,
            cap_web_search: false,
            web_search_tool: None,
            cap_system_msg: false,
            reasoning_levels: vec![],
            default_reasoning_level: None,
            reasoning_labels: Default::default(),
            reasoning_params: Default::default(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub id: String,
    pub name: String,
    pub base_url: String,
    /// 支持 ${ENV_VAR} 引用环境变量
    pub api_key: String,
    pub api_format: ApiFormat,
    pub enabled: bool,
    pub models: Vec<ModelConfig>,
}

/// 回合 token 用量：input = 未缓存命中的输入，cache_read = 缓存命中的输入
///（命中率 = cache_read / (input + cache_read)），output = 输出；
/// duration_ms 为回合墙钟耗时（含工具执行/审批等待），api_ms 为纯 provider
/// 请求耗时，ttft_ms 为其中等待首个输出 token 的时间之和、api_steps 为请求
/// 次数（平均首字 = ttft_ms / api_steps）；输出速度按 (api_ms - ttft_ms)
/// 计算，即不含首字的纯解码速度
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TurnUsageStats {
    pub input: u64,
    pub cache_read: u64,
    pub output: u64,
    pub duration_ms: u64,
    #[serde(default)]
    pub api_ms: u64,
    #[serde(default)]
    pub ttft_ms: u64,
    #[serde(default)]
    pub api_steps: u64,
}

/// models.dev 的模型元数据（自动填充用）
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelRegistryInfo {
    /// 数据源里的完整 ID（provider/model）
    pub full_id: String,
    /// 上下文窗口（limit.context，缺省回退 limit.input）
    pub context: Option<u64>,
    /// 单独的最大输入限制（多数模型没有）
    pub input: Option<u64>,
    /// 最大输出 token（limit.output）
    pub output: Option<u64>,
    /// 是否支持推理
    pub reasoning: bool,
    /// 推理等级（reasoning_options 里 effort 类型的 values；仅 toggle 的为空）
    pub reasoning_levels: Vec<String>,
    /// 输入模态（modalities.input，如 ["text","image"]）；空 = 数据源未给，UI 不动能力勾选
    #[serde(default)]
    pub input_modalities: Vec<String>,
    /// 结构化输出支持；None = 数据源未给，UI 不动勾选
    #[serde(default)]
    pub structured_output: Option<bool>,
}

/// models.dev 只给等级名，参数形态按供应商 API 格式生成（对齐 ZCode 内置规则）：
/// 等级名直接透传（不归一）；OpenAI Chat → 四字段兼容包（thinking/enable_thinking/
/// reasoning_effort/reasoning.effort，不同后端认不同字段）；Anthropic Messages →
/// 关档 thinking.type=disabled，开档 thinking.type=enabled + output_config.effort
/// （GLM-5/DeepSeek-V4/Claude-5 这代模型的形态；不使用 budget_tokens）。
pub fn default_reasoning_params(
    levels: &[String],
    api_format: ApiFormat,
) -> serde_json::Map<String, serde_json::Value> {
    let mut map = serde_json::Map::new();
    for level in levels {
        let off = level == "none" || level == "disabled";
        let params = match api_format {
            ApiFormat::OpenAiChat => {
                // "enabled" 是开关型等级，落到 effort high（ZCode 兜底同款）
                let effort = if off {
                    "none"
                } else if level == "enabled" {
                    "high"
                } else {
                    level.as_str()
                };
                serde_json::json!({
                    "thinking": { "type": if off { "disabled" } else { "enabled" } },
                    "enable_thinking": !off,
                    "reasoning_effort": effort,
                    "reasoning": { "effort": effort },
                })
            }
            ApiFormat::AnthropicMessages => {
                if off {
                    serde_json::json!({ "thinking": { "type": "disabled" } })
                } else {
                    let effort = if level == "enabled" {
                        "high"
                    } else {
                        level.as_str()
                    };
                    serde_json::json!({
                        "thinking": { "type": "enabled" },
                        "output_config": { "effort": effort },
                    })
                }
            }
        };
        map.insert(level.clone(), params);
    }
    map
}

#[cfg(test)]
mod reasoning_params_tests {
    use super::*;

    #[test]
    fn openai_params_pass_level_through_with_compat_fields() {
        let levels: Vec<String> = ["none", "low", "max", "enabled"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let params = default_reasoning_params(&levels, ApiFormat::OpenAiChat);
        // none：四字段全关档
        assert_eq!(params["none"]["thinking"]["type"], "disabled");
        assert_eq!(params["none"]["enable_thinking"], false);
        assert_eq!(params["none"]["reasoning_effort"], "none");
        // 等级透传，max 不归一
        assert_eq!(params["low"]["reasoning_effort"], "low");
        assert_eq!(params["max"]["reasoning_effort"], "max");
        assert_eq!(params["max"]["reasoning"]["effort"], "max");
        assert_eq!(params["max"]["thinking"]["type"], "enabled");
        // 开关型等级 enabled → effort high
        assert_eq!(params["enabled"]["reasoning_effort"], "high");
    }

    #[test]
    fn anthropic_params_use_output_config_effort_without_budget() {
        let levels: Vec<String> = ["none", "low", "max"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let params = default_reasoning_params(&levels, ApiFormat::AnthropicMessages);
        assert_eq!(params["none"]["thinking"]["type"], "disabled");
        assert!(params["none"].get("output_config").is_none());
        assert_eq!(params["low"]["thinking"]["type"], "enabled");
        assert_eq!(params["low"]["output_config"]["effort"], "low");
        // max 透传，不映射预算
        assert_eq!(params["max"]["output_config"]["effort"], "max");
        let flat = serde_json::to_string(&params).unwrap();
        assert!(!flat.contains("budget_tokens"), "不再生成 budget_tokens");
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AppConfig {
    pub providers: Vec<ProviderConfig>,
    pub default_provider: String,
    pub default_model: String,
}

/// 工作区条目（store.sqlite workspaces 表持久化）
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkspaceMeta {
    pub path: PathBuf,
    pub added_at: u64,
    /// 用户自定义显示名（重命名）；None 使用目录名
    #[serde(default)]
    pub alias: Option<String>,
    /// 已从侧栏移除（隐藏）；该工作区下新建会话时自动恢复
    #[serde(default)]
    pub hidden: bool,
}

/// 会话列表元数据（store.sqlite sessions 表持久化）
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    pub title: String,
    pub cwd: PathBuf,
    pub created_at: u64,
    pub updated_at: u64,
    pub pinned: bool,
    pub archived: bool,
    /// 标题被手动重命名过：自动命名（首条消息模型生成）不再覆盖
    #[serde(default)]
    pub title_custom: bool,
    /// 会话最近使用的模型/思考等级/执行模式（重开恢复；新会话继承工作区最近活跃值）
    #[serde(default)]
    pub provider_id: Option<String>,
    #[serde(default)]
    pub model_id: Option<String>,
    #[serde(default)]
    pub reasoning_level: Option<String>,
    #[serde(default)]
    pub exec_mode: ExecMode,
    /// 会话级开关：允许读取/写入工作区外文件（tmp 目录始终放行；敏感文件永远拦截）
    #[serde(default)]
    pub fs_read_outside: bool,
    #[serde(default)]
    pub fs_write_outside: bool,
}

/// TodoList 工具的待办项：会话级状态，写入时整体替换。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TodoItem {
    pub content: String,
    pub status: TodoStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Done,
}

/// 后台 Bash 任务状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskStatus {
    Running,
    Exited(i32),
    Killed,
}

/// 后台任务面板快照（output_tail 为输出尾部节选）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskSummary {
    pub id: String,
    pub command: String,
    pub status: TaskStatus,
    pub started_at: u64,
    pub ended_at: Option<u64>,
    pub output_tail: String,
}

/// AskUserQuestion 的选项。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QuestionOption {
    pub label: String,
    #[serde(default)]
    pub description: Option<String>,
}

/// AskUserQuestion 的单题。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QuestionItem {
    pub question: String,
    #[serde(default)]
    pub header: Option<String>,
    #[serde(default)]
    pub multi_select: bool,
    pub options: Vec<QuestionOption>,
}

/// core → UI 事件
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Event {
    SessionConfigured {
        session_id: String,
        cwd: PathBuf,
        model: String,
        provider_name: String,
        /// 会话当前的模型/思考等级/执行模式（重开恢复、新建继承的值）
        provider_id: Option<String>,
        model_id: Option<String>,
        reasoning_level: Option<String>,
        exec_mode: ExecMode,
        /// 会话级「工作区外读/写」开关（UI 模式菜单勾选态恢复用）
        #[serde(default)]
        fs_read_outside: bool,
        #[serde(default)]
        fs_write_outside: bool,
    },
    SessionList {
        sessions: Vec<SessionMeta>,
    },
    /// 会话标题变化（自动命名 sidecar 完成 / 手动重命名后的列表同步）
    SessionTitleChanged {
        session_id: String,
        title: String,
    },
    WorkspaceList {
        workspaces: Vec<WorkspaceMeta>,
    },
    ConfigSnapshot {
        config: AppConfig,
    },
    TestResult {
        provider_id: String,
        ok: bool,
        message: String,
    },
    /// models.dev 模型元数据查询结果；info=None = 数据源里没有该 ID
    ModelInfo {
        id: String,
        info: Option<ModelRegistryInfo>,
    },
    FileSearchResults {
        session_id: String,
        query: String,
        results: Vec<String>,
    },
    GitInfo {
        cwd: PathBuf,
        current_branch: Option<String>,
        branches: Vec<String>,
    },
    BranchChanged {
        cwd: PathBuf,
        branch: String,
    },
    GitStatus {
        cwd: PathBuf,
        /// false = 非 git 仓库（两列表为空）
        is_git: bool,
        unstaged: Vec<GitFileChange>,
        staged: Vec<GitFileChange>,
    },
    GitDiff {
        cwd: PathBuf,
        path: String,
        staged: bool,
        diff: String,
    },
    /// 会话回合进行中到达的消息已排队；回合结束后自动接续
    MessageQueued {
        session_id: String,
        seq: u64,
        text: String,
    },
    /// resume 重放用户消息（新建消息不走事件，UI 本地追加）
    UserMessage {
        session_id: String,
        seq: u64,
        text: String,
        files: Vec<String>,
    },
    TurnStarted {
        session_id: String,
        seq: u64,
        turn_id: String,
    },
    /// 思考增量（GLM/DeepSeek 的 reasoning_content），live-only
    ReasoningDelta {
        session_id: String,
        seq: u64,
        item_id: String,
        delta: String,
    },
    TextDelta {
        session_id: String,
        seq: u64,
        item_id: String,
        delta: String,
    },
    /// 文本终值，可回放边界
    TextDone {
        session_id: String,
        seq: u64,
        item_id: String,
        full_text: String,
    },
    ToolCallBegin {
        session_id: String,
        seq: u64,
        item_id: String,
        tool: String,
        input_summary: String,
        /// 完整参数 JSON（审批卡展示用）
        detail: String,
    },
    ToolCallEnd {
        session_id: String,
        seq: u64,
        item_id: String,
        output: String,
        is_error: bool,
        /// 写/改类工具的本次编辑 diff（卡片内联渲染用；会话累计 diff 走 FileChanged）
        #[serde(default)]
        edit: Option<EditDiff>,
    },
    ContextUsage {
        session_id: String,
        seq: u64,
        used: u64,
        total: u64,
        /// 会话累计（含 resume 恢复）：缓存命中的输入 token 与未命中的输入 token，
        /// 平均缓存命中率 = cache_read_total / (cache_read_total + input_total)
        #[serde(default)]
        cache_read_total: u64,
        #[serde(default)]
        input_total: u64,
    },
    ContextCompacted {
        session_id: String,
        seq: u64,
        omitted: usize,
        note: String,
        /// true = 采样前自动触发；false = 用户手动 /compact
        automatic: bool,
    },
    /// core 阻塞等待 Op::ApprovalReply（同 request_id）
    ApprovalRequested {
        session_id: String,
        seq: u64,
        request_id: String,
        tool: String,
        /// Bash: 完整命令；Write/Edit: 路径 + diff 预览
        detail: String,
    },
    /// core 侧主动切换了执行模式（ExitPlanMode 确认后）：UI 同步模式 chip
    ExecModeChanged {
        session_id: String,
        seq: u64,
        mode: ExecMode,
    },
    /// AskUserQuestion：core 阻塞等待 Op::QuestionReply（同 request_id）
    QuestionRequested {
        session_id: String,
        seq: u64,
        request_id: String,
        questions: Vec<QuestionItem>,
    },
    FileChanged {
        session_id: String,
        seq: u64,
        path: String,
        unified_diff: String,
        additions: u32,
        deletions: u32,
    },
    FileReverted {
        session_id: String,
        seq: u64,
        path: String,
    },
    /// TodoList 工具写入成功后的待办快照（含新建/切换会话时的初始空快照）
    TodoListChanged {
        session_id: String,
        seq: u64,
        items: Vec<TodoItem>,
    },
    /// 后台任务状态变化后的面板快照（启动/退出/停止）
    TaskListChanged {
        session_id: String,
        seq: u64,
        tasks: Vec<TaskSummary>,
    },
    TurnComplete {
        session_id: String,
        seq: u64,
        duration_ms: u64,
        /// 回合 token 统计（中断/无用量数据的回合为 None）
        #[serde(default)]
        stats: Option<TurnUsageStats>,
    },
    TurnAborted {
        session_id: String,
        seq: u64,
    },
    /// 一轮结束：本轮 agent 的文件改动（每文件 本轮首次写前 → 当前 的净 diff，ZCode turn 头部面板口径）
    TurnFileChanges {
        session_id: String,
        seq: u64,
        files: Vec<EditDiff>,
    },
    Error {
        session_id: Option<String>,
        seq: u64,
        message: String,
    },
}
