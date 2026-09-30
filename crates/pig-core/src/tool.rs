use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::Ordering;

use pig_protocol::ExecMode;

use crate::provider::ToolCall;
use crate::task::SessionToolState;
use crate::text::{FileEncoding, LineEnding};

const MAX_READ_LINES: usize = 2000;
const MAX_READ_CHARS: usize = 100_000;
/// Read 单行字符上限，超过则截断该行
const MAX_LINE_CHARS: usize = 2000;
const MAX_MATCH_RESULTS: usize = 200;
/// Bash 前台输出字符上限，超过则头尾预览 + 完整输出留 spill 文件
const MAX_BASH_OUTPUT: usize = 30 * 1024;
const MAX_GREP_FILE_SIZE: u64 = 2 * 1024 * 1024;
/// Grep 匹配行字符上限（对标 rg --max-columns）：minified JS/单行大 JSON
/// 一次命中就能打爆上下文，超长行截断显示
const MAX_GREP_LINE_CHARS: usize = 500;
/// Grep/Glob 输出总字符预算：上下文行会放大输出（200 命中 × 11 行窗口），
/// 预算内先到先得，超了停止扫描并提示分页
const MAX_GREP_OUTPUT_CHARS: usize = 30 * 1024;

/// Grep 输出行截断（超 500 字符加标注）
fn truncate_grep_line(line: &str) -> String {
    if line.chars().count() > MAX_GREP_LINE_CHARS {
        let taken: String = line.chars().take(MAX_GREP_LINE_CHARS).collect();
        format!("{taken} [...行超长已截断]")
    } else {
        line.to_string()
    }
}
/// Read 读盘前的文件体积上限：防止「先整个读进内存再做输出预算」撑爆内存
const MAX_READ_FILE_BYTES: u64 = 100 * 1024 * 1024;
/// Edit 上限（整读+整写，比 Read 更保守）
const MAX_EDIT_FILE_BYTES: u64 = 50 * 1024 * 1024;

/// 体积护栏的报错构造：超上限返回 Some(文案)，调用方各自负责 stat 与 NotFound 文案。
fn file_size_error(len: u64, cap: u64, hint: &str) -> Option<String> {
    (len > cap).then(|| {
        format!(
            "文件过大（{} MB，超过 {} MB 上限）；{hint}",
            len / 1024 / 1024,
            cap / 1024 / 1024
        )
    })
}

pub struct FileChange {
    pub path: String,
    pub unified_diff: String,
    pub additions: u32,
    pub deletions: u32,
}

/// 工具输出的图片（ReadMediaFile）：随 history 进模型上下文（Anthropic blocks /
/// OpenAI 拆 user 消息），不进 protocol、不落 rollout
pub struct ToolImage {
    pub media_type: String,
    pub data_base64: String,
    pub width: u32,
    pub height: u32,
}

pub struct ToolEffect {
    pub output: String,
    pub file_change: Option<FileChange>,
    /// 本次编辑自身的 diff（「编辑前 → 编辑后」），UI 工具卡片内联渲染用；
    /// `file_change` 是会话累计口径（review 面板用），两者粒度不同
    pub edit_diff: Option<FileChange>,
    /// 图片输出（ReadMediaFile）；其余工具恒为空
    pub images: Vec<ToolImage>,
}

impl ToolEffect {
    fn plain(output: String) -> Self {
        Self {
            output,
            file_change: None,
            edit_diff: None,
            images: vec![],
        }
    }
}

impl From<FileChange> for pig_protocol::EditDiff {
    fn from(change: FileChange) -> Self {
        Self {
            path: change.path,
            unified_diff: change.unified_diff,
            additions: change.additions,
            deletions: change.deletions,
        }
    }
}

/// 计算单次编辑的 unified diff（编辑前 → 编辑后），相对路径归一化为 `/`。
pub(crate) fn per_edit_diff(cwd: &Path, full: &Path, before: &str, after: &str) -> FileChange {
    let diff = similar::TextDiff::from_lines(before, after);
    let mut additions = 0;
    let mut deletions = 0;
    for change in diff.iter_all_changes() {
        match change.tag() {
            similar::ChangeTag::Insert => additions += 1,
            similar::ChangeTag::Delete => deletions += 1,
            _ => {}
        }
    }
    let cwd_canonical = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let relative = full
        .strip_prefix(&cwd_canonical)
        .or_else(|_| full.strip_prefix(cwd))
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|_| full.to_path_buf());
    let relative = relative.to_string_lossy().replace('\\', "/");
    let unified = diff
        .unified_diff()
        .context_radius(3)
        .header(&format!("a/{relative}"), &format!("b/{relative}"))
        .to_string();
    FileChange {
        path: relative,
        unified_diff: unified,
        additions,
        deletions,
    }
}

/// 会话级变更追踪：首次修改前快照原始字节，diff 始终是「原始 → 当前」（LF 视图）。
/// 快照经 dirty 标记由 session 侧落盘（file_originals 表），重启后 restore 恢复基线。
/// 快照存原始字节（非 String），GBK/UTF-16/二进制都能字节级 revert；
/// 持久化边界仍是 String，经 snapshot_to_store/from_store 转换（非 UTF-8 走 hex）。
///
/// 另有一层**每轮**追踪（ZCode turn-file-changes 同款口径）：turn 内首次写前
mod bash;
mod bash_policy;
mod edit;
mod fetch;
mod media;
mod misc;
mod paths;
mod read;
mod search;
mod skill;
mod tracker;
mod websearch;
mod write;
pub(crate) use write::atomic_write;

// 子模块整体提升到 crate 可见（根模块的 registry/工具间互引用）；对外 API
// 的可见性以原先为准，由下方显式 pub use 钉住（调用点零改动）。
pub(crate) use fetch::*;
pub(crate) use media::*;
pub(crate) use misc::*;
pub(crate) use paths::*;
pub(crate) use read::*;
pub(crate) use tracker::*;
pub(crate) use websearch::*;

pub use bash::is_dangerous_command;
pub use bash_policy::is_readonly_command;
pub use edit::{EditMatchError, EditOutcome, compute_edit};
pub use fetch::{check_fetch_url, extract_text, is_private_host, is_private_ip};
pub use media::{
    base64_encode, compress_image_for_model, convert_tiff_to_png, decode_image_check,
    encode_image_for_model, image_dimensions, sniff_image,
};
pub use misc::{AgentSwarmTool, AgentTool, parse_questions, parse_swarm_args};
pub use paths::{is_sensitive_file, resolve_checked, resolve_with_access};
pub use search::search_files;
pub(crate) use skill::SkillTool;
pub use tracker::{ChangeTracker, snapshot_from_store, snapshot_to_store};

pub use pig_protocol::{TodoItem, TodoStatus};

/// 会话共享的待办清单（Arc 句柄，Session 与 ToolContext 共用）。
pub type TodoHandle = std::sync::Arc<std::sync::Mutex<Vec<TodoItem>>>;

pub struct ToolContext<'a> {
    pub cwd: &'a Path,
    pub tracker: &'a mut ChangeTracker,
    pub state: &'a crate::task::SessionToolState,
}

pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn schema(&self) -> serde_json::Value;
    /// 只读工具在任何模式下都免审批
    fn read_only(&self) -> bool {
        false
    }
    fn is_shell(&self) -> bool {
        false
    }
    fn execute<'a>(
        &'a self,
        args: serde_json::Value,
        ctx: ToolContext<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<ToolEffect, String>> + Send + 'a>>;
}

pub fn all() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(ReadFile),
        Box::new(WriteFile),
        Box::new(EditFile),
        Box::new(Glob),
        Box::new(Grep),
        Box::new(Bash),
        Box::new(TodoListTool),
        Box::new(FetchUrl),
        Box::new(WebSearch),
        Box::new(TaskList),
        Box::new(TaskOutput),
        Box::new(TaskStop),
        Box::new(AskUserQuestionTool),
        Box::new(ExitPlanModeTool),
        Box::new(EnterPlanModeTool),
        Box::new(ReadMediaFile),
    ]
}

pub fn schemas() -> Vec<serde_json::Value> {
    all().iter().map(|tool| tool.schema()).collect()
}

/// 根会话工具集 = 全部内置工具 + Agent/AgentSwarm + Skill（子代理循环用
/// all() 收窄后在 spawn 点单独补 Skill，天然无 Agent 防嵌套）。
/// profiles 由调用方传**会话冻结快照**：档案清单内嵌在 Agent/AgentSwarm 的
/// description 里，每步重扫磁盘会让编辑子代理档案打断 tools 前缀缓存
/// （tools 在缓存前缀最前面，变了从第 0 字节起全量失效；kimi
/// frozenCatalogProfiles / ZCode 启动装配同款取舍）。spawn 执行时另走
/// load_profiles 现读，清单过期由"档案不存在"报错自愈
pub fn all_root(
    cwd: &Path,
    data_dir: &Path,
    profiles: &[crate::agent::AgentProfile],
) -> Vec<Box<dyn Tool>> {
    let mut tools = all();
    tools.push(Box::new(AgentTool::new(profiles)));
    tools.push(Box::new(AgentSwarmTool::new(profiles)));
    tools.push(Box::new(SkillTool::new(cwd, data_dir)));
    tools
}

/// 根会话工具 schema 集（含 Agent）；入参口径与 all_root 一致
pub fn schemas_root(
    cwd: &Path,
    data_dir: &Path,
    profiles: &[crate::agent::AgentProfile],
) -> Vec<serde_json::Value> {
    all_root(cwd, data_dir, profiles)
        .iter()
        .map(|tool| tool.schema())
        .collect()
}

/// 审批判定：Plan 模式在更早处拦截（直接拒绝），这里只管其余档。
/// Yolo 与 FullAccess 都不审批（危险命令强制弹窗在 session 层，Yolo 在那里也跳过）。
pub fn requires_approval(tool: &dyn Tool, mode: ExecMode) -> bool {
    // Agent/AgentSwarm 一律免审批：子代理内部每个写操作会自己走审批门，
    // 不对委派调用本身二次审批（弹窗文案也没法描述整个子任务）
    if tool.name() == "Agent" || tool.name() == "AgentSwarm" {
        return false;
    }
    match mode {
        ExecMode::FullAccess | ExecMode::Yolo => false,
        ExecMode::Plan => false,
        ExecMode::ConfirmBeforeEdit => !tool.read_only(),
        // MCP 工具无 annotations 时按非只读保守处理：AutoEdit 下也弹审批
        //（is_shell 只覆盖 Bash，挡不住 MCP 写工具直通）
        ExecMode::AutoEdit => {
            tool.is_shell() || (tool.name().starts_with("mcp__") && !tool.read_only())
        }
    }
}

pub fn summarize(call: &ToolCall) -> String {
    let args: serde_json::Value = serde_json::from_str(&call.arguments).unwrap_or_default();
    let raw = match call.name.as_str() {
        "Read" | "Write" | "Edit" | "ReadMediaFile" => {
            args["path"].as_str().unwrap_or("?").to_string()
        }
        "Bash" => args["command"].as_str().unwrap_or("?").to_string(),
        "Glob" => args["pattern"].as_str().unwrap_or("?").to_string(),
        "Grep" => args["pattern"].as_str().unwrap_or("?").to_string(),
        "TodoList" => args["todos"]
            .as_array()
            .map(|items| format!("更新待办（{} 项）", items.len()))
            .unwrap_or_else(|| "查看待办".to_string()),
        "FetchURL" => args["url"].as_str().unwrap_or("?").to_string(),
        "WebSearch" => args["query"].as_str().unwrap_or("?").to_string(),
        "Skill" => args["skill"].as_str().unwrap_or("?").to_string(),
        "TaskList" => "列出后台任务".to_string(),
        "TaskOutput" | "TaskStop" => args["task_id"].as_str().unwrap_or("?").to_string(),
        "AskUserQuestion" => args["questions"][0]["question"]
            .as_str()
            .unwrap_or("?")
            .to_string(),
        "ExitPlanMode" => "请求退出计划模式".to_string(),
        "EnterPlanMode" => "请求进入计划模式".to_string(),
        "Agent" => format!(
            "子代理 {}: {}",
            args["subagent_type"]
                .as_str()
                .or_else(|| args["resume"].as_str())
                .unwrap_or("general-purpose"),
            args["description"].as_str().unwrap_or("?")
        ),
        "AgentSwarm" => format!(
            "子代理群（{} 项）",
            args["items"].as_array().map(|a| a.len()).unwrap_or(0)
                + args["resume_agent_ids"]
                    .as_object()
                    .map(|m| m.len())
                    .unwrap_or(0)
        ),
        _ => args.to_string(),
    };
    // 不在源头截断：折叠行由 UI 做单行省略，展开卡片要完整显示；
    // 全文本就在 arguments 里随 rollout 持久化，摘要不再额外截短
    raw
}

/// 兼容入口：仅内置工具（集成测试用）；运行时路径（门控/并发只读段）走 execute_with_extra
pub async fn execute(
    call: &ToolCall,
    ctx: ToolContext<'_>,
) -> (
    String,
    bool,
    Option<FileChange>,
    Option<pig_protocol::EditDiff>,
    Vec<ToolImage>,
) {
    execute_with_extra(call, ctx, &[]).await
}

pub async fn execute_with_extra(
    call: &ToolCall,
    ctx: ToolContext<'_>,
    // 内置以外的运行时工具（MCP）：按名查找的兜底清单
    extra: &[Box<dyn Tool>],
) -> (
    String,
    bool,
    Option<FileChange>,
    Option<pig_protocol::EditDiff>,
    Vec<ToolImage>,
) {
    let args: serde_json::Value = serde_json::from_str(&call.arguments).unwrap_or_default();
    let tools = all();
    let Some(tool) = tools
        .iter()
        .chain(extra.iter())
        .find(|tool| tool.name() == call.name)
    else {
        return (format!("未知工具: {}", call.name), true, None, None, vec![]);
    };
    match tool.execute(args, ctx).await {
        Ok(effect) => (
            effect.output,
            false,
            effect.file_change,
            effect.edit_diff.map(Into::into),
            effect.images,
        ),
        Err(error) => (error, true, None, None, vec![]),
    }
}

/// resolve_core 的越界形态（供策略层区分处理；文案由调用方定）
enum BoundFailure {
    /// 父目录 canonical 后在工作区外
    ParentOutside {
        resolved: PathBuf,
    },
    /// 目标已存在（含符号链接），canonical 后在工作区外
    TargetOutside {
        resolved: PathBuf,
    },
    /// 悬空符号链接：fail-closed，策略层也不放行
    DanglingSymlink,
    Other(String),
}

struct ReadFile;
struct WriteFile;
struct EditFile;
struct Glob;
struct Grep;
struct Bash;

pub fn approval_subject(call: &ToolCall) -> String {
    let args: serde_json::Value = serde_json::from_str(&call.arguments).unwrap_or_default();
    match call.name.as_str() {
        "Bash" => args["command"]
            .as_str()
            .and_then(|command| command.split_whitespace().next())
            .unwrap_or("")
            .to_string(),
        "Write" | "Edit" => args["path"].as_str().unwrap_or("").to_string(),
        _ => String::new(),
    }
}
