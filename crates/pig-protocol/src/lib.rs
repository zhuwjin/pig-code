//! pig-code 的 UI 与 core 之间的契约类型（见 docs/PLAN.md §2.2）。
//! 纯 serde 类型，无业务逻辑；UI 与 core 双向依赖本 crate。
//!
//! 原则：delta 事件（流式碎片，live-only）与 done 事件（全量终值，可回放）分离；
//! 会话事件带 session_id + seq（per-session 单调递增）。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// 发送消息时附带的图片（剪贴板粘贴）：进程内通道，原始字节 + mime。
/// core 侧压缩后进模型上下文；落盘持久化用 rollout 的 ImageRef（不存 base64）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PendingImage {
    pub bytes: Vec<u8>,
    pub mime: String,
}

mod config;
mod event;
mod op;

pub use config::*;
pub use event::*;
pub use op::*;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecMode {
    #[default]
    ConfirmBeforeEdit,
    AutoEdit,
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
    /// 计划模式（与执行模式正交的独立开关，ZCode planEnabled 同语义）：
    /// 开启时修改类工具被硬拒，ExitPlanMode 批准只翻转本开关、权限档不动
    #[serde(default)]
    pub plan_enabled: bool,
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
    /// 子代理（Agent）任务为 Some(agent_id)，Bash 后台任务为 None——
    /// UI 按此把任务 chip/弹层拆成「后台 Bash / 后台 Agent」两类
    pub agent_id: Option<String>,
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

/// 子代理对话的只读展示行（Event::SubagentHistory 载荷）
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SubagentItem {
    /// "user" | "assistant" | "tool"
    pub role: String,
    /// user/assistant 正文；tool 为调用摘要
    pub text: String,
    /// tool 行的工具名
    pub tool: Option<String>,
    /// tool 行输出（core 侧截断 2000 字符，字符边界）
    pub output: Option<String>,
    pub is_error: bool,
}
