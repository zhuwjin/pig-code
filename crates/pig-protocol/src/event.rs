use super::*;

/// 单个 MCP server 的连接状态快照（设置页展示用）：
/// 连接成功带工具数；连接失败 connected=false 且 error 为原因
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerStatus {
    pub name: String,
    pub connected: bool,
    pub tool_count: usize,
    pub error: Option<String>,
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
    /// Op::ListMcpServers 的应答：该会话的 MCP server 状态清单
    ///（None = 会话不存在或尚未发起懒连接——MCP 在首个回合采样前才连）
    McpServerList {
        session_id: String,
        servers: Option<Vec<McpServerStatus>>,
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
        /// 附带的图片张数（协议兼容保留；UI 展示用 text 末尾的
        /// pig-code-composer://attachments/mN 链接，字节不在事件里）
        #[serde(default)]
        image_count: usize,
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
    /// 子代理实时进度（live-only，不落 rollout）：item_id = 父会话 Agent 工具卡片
    SubagentProgress {
        session_id: String,
        seq: u64,
        item_id: String,
        note: String,
    },
    /// 子代理工具卡的「代理卡」元信息（live 直发 + rollout 持久化，回放经记录重建）：
    /// item_id = 父会话 Agent 工具卡片；无此元信息（live 中本事件到达前的瞬时态）
    /// 时 UI 按标准工具卡样式渲染
    SubagentCard {
        session_id: String,
        seq: u64,
        item_id: String,
        agent_id: String,
        profile: String,
        description: String,
        /// "{provider_name} · {model}"（档案带 thought_level 时追加「 · {level}」）
        model: String,
        /// 本次运行为后台（run_in_background）：后台卡的运行态由子代理真实
        /// 生命周期（SubagentActivity）驱动，而非工具调用的 done
        background: bool,
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
    /// 压缩开始（手动 /compact 与采样前水位自动触发都会发；摘要是一次性阻塞
    /// 请求，期间回合无其他事件，UI 靠它显示「正在压缩上下文」）
    CompactStarted {
        session_id: String,
        seq: u64,
        /// true = 采样前自动触发；false = 用户手动 /compact
        automatic: bool,
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
    /// Op::LoadSubagent 的应答：子代理完整对话的只读投影（右侧「子代理」tab）
    SubagentHistory {
        session_id: String,
        seq: u64,
        agent_id: String,
        /// meta.description（tab 标题/卡片标题用）
        title: String,
        /// "{provider} · {model}"
        subtitle: String,
        items: Vec<SubagentItem>,
        /// 加载时刻子代理仍在运行（任务注册表口径）：面板显示「运行中」指示
        #[serde(default)]
        running: bool,
    },
    /// 子代理实时展示项（live-only）：子代理每完成一批消息逐条发出；
    /// item=None + finished=true 表示子代理结束（含取消/被杀），面板关「运行中」
    SubagentActivity {
        session_id: String,
        seq: u64,
        agent_id: String,
        item: Option<SubagentItem>,
        finished: bool,
    },
    Error {
        session_id: Option<String>,
        seq: u64,
        message: String,
    },
}
