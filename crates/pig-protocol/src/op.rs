use super::*;

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
        /// 计划模式开关（None/false = 关；不种子继承——计划是临时态）
        #[serde(default)]
        plan_enabled: Option<bool>,
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
    /// 会话分叉：以源会话前 N 个回合的历史派生新会话（boundary 含第 N 回合
    /// 本身），创建后按 OpenSession 冷路径打开（SessionConfigured + replay）
    ForkSession {
        session_id: String,
        /// 保留的回合数（≥1，0 钳为 1；超过源回合总数 = 全量复制）
        turns: usize,
    },
    SendMessage {
        session_id: String,
        content: String,
        files: Vec<String>,
        /// 粘贴/拖拽进来的图片（原始字节，core 侧压缩后进模型上下文）
        #[serde(default)]
        images: Vec<PendingImage>,
        mode: ExecMode,
    },
    Interrupt {
        session_id: String,
    },
    ApprovalReply {
        request_id: String,
        decision: ApprovalDecision,
        /// 反馈意见（kimi Revise：拒绝计划时携带给模型修订；仅 ExitPlanMode 用）
        #[serde(default)]
        feedback: Option<String>,
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
    /// 计划模式开关（与执行模式正交；模型侧经 EnterPlanMode/ExitPlanMode 自切）
    SetPlanMode {
        session_id: String,
        enabled: bool,
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
        /// 显式搜索目录（hero 未建会话时传 hero 工作区；None = 按会话/默认目录）
        #[serde(default)]
        cwd: Option<String>,
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
    /// 加载后台子代理的完整对话（右侧「子代理」tab 只读展示）；
    /// 无需 Session 实例，直接读 {sessions_dir}/{session_id}.agents/{agent_id}.jsonl
    LoadSubagent {
        session_id: String,
        agent_id: String,
    },
    /// 非会话态：按模型 ID 查 models.dev 元数据（上下文/输入输出上限/推理等级）；
    /// 磁盘缓存命中直接回，未命中（新模型）重新拉取后再回
    ModelLookup {
        id: String,
    },
    Compact {
        session_id: String,
        /// 用户对本次摘要的特别要求（/compact 选中后在输入框续写的重点说明；
        /// 对齐 ZCode 自定义指令 / kimi-code custom_instruction_block）
        #[serde(default)]
        instruction: Option<String>,
    },
    /// 查询会话的 MCP server 连接清单（设置页展示用）；回 Event::McpServerList
    ListMcpServers {
        session_id: String,
    },
    Shutdown,
}
