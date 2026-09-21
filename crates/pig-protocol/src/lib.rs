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
        title: Option<String>,
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
    SetModel {
        session_id: String,
        provider_id: String,
        model_id: String,
        /// None = 不启用推理参数
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
    /// 取消该会话队首的排队消息（FIFO）
    CancelQueued {
        session_id: String,
        text: String,
    },
    Compact {
        session_id: String,
    },
    Shutdown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecMode {
    ConfirmBeforeEdit,
    AutoEdit,
    Plan,
    FullAccess,
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

/// core → UI 事件
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Event {
    SessionConfigured {
        session_id: String,
        cwd: PathBuf,
        model: String,
        provider_name: String,
    },
    SessionList {
        sessions: Vec<SessionMeta>,
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
    },
    ContextUsage {
        session_id: String,
        seq: u64,
        used: u64,
        total: u64,
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
    },
    TurnAborted {
        session_id: String,
        seq: u64,
    },
    Error {
        session_id: Option<String>,
        seq: u64,
        message: String,
    },
}
