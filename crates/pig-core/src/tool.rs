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

/// 回放没有 edit 字段的旧 rollout 记录时，从参数兜底重建本次编辑 diff
///（ZCode 前端兜底同款思路）：Edit 用 old_string/new_string 现算；
/// Write 按新建文件处理（before 为空 → 全量新增）。其余工具返回 None。
pub fn fallback_edit_diff(
    cwd: &Path,
    tool: &str,
    arguments: &str,
) -> Option<pig_protocol::EditDiff> {
    let args: serde_json::Value = serde_json::from_str(arguments).ok()?;
    let path = args["path"].as_str()?;
    let (before, after) = match tool {
        "Edit" => (
            args["old_string"].as_str()?.to_string(),
            args["new_string"].as_str()?.to_string(),
        ),
        "Write" => (String::new(), args["content"].as_str()?.to_string()),
        _ => return None,
    };
    Some(per_edit_diff(cwd, &cwd.join(path), &before, &after).into())
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
/// 记录 turn_originals，回合结束 take_turn_changes 算「本轮首次写前 → 当前」净额并清空。
#[derive(Default)]
pub struct ChangeTracker {
    originals: HashMap<PathBuf, Option<Vec<u8>>>,
    stats: HashMap<PathBuf, (u32, u32)>,
    /// 本次进程内新增、尚未落盘的快照路径（session 侧 drain 后写库）
    dirty: Vec<PathBuf>,
    /// 本轮内各文件首次写前的原始字节（None = 本轮新建）；回合结束 take 清空
    turn_originals: HashMap<PathBuf, Option<Vec<u8>>>,
}

/// 读文件原始字节（None = 文件不存在）
fn read_original_bytes(path: &Path) -> Result<Option<Vec<u8>>, String> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("读取失败 {}: {e}", path.display())),
    }
}

/// 字节 → LF 模型视图：优先 text::decode，失败降级 lossy UTF-8（diff 兜底用）
fn decoded_view(bytes: &[u8]) -> String {
    match crate::text::decode(bytes) {
        Ok(doc) => doc.text,
        Err(_) => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// 快照持久化编码前缀：非 UTF-8 字节以十六进制度过 String 边界
const SNAPSHOT_HEX_PREFIX: &str = "pigcode:hex:";

/// 原始字节 → 持久化 String：合法 UTF-8 直接转；否则 hex（保持 store/session 签名不变）
pub fn snapshot_to_store(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_string(),
        Err(_) => {
            let mut out = String::with_capacity(SNAPSHOT_HEX_PREFIX.len() + bytes.len() * 2);
            out.push_str(SNAPSHOT_HEX_PREFIX);
            for byte in bytes {
                out.push(char::from_digit((byte >> 4) as u32, 16).expect("0-15 是合法 hex 位"));
                out.push(char::from_digit((byte & 0x0f) as u32, 16).expect("0-15 是合法 hex 位"));
            }
            out
        }
    }
}

/// 持久化 String → 原始字节：有 hex 前缀则解码，否则按 UTF-8 字节（兼容旧数据）
pub fn snapshot_from_store(s: &str) -> Vec<u8> {
    let Some(hex) = s.strip_prefix(SNAPSHOT_HEX_PREFIX) else {
        return s.as_bytes().to_vec();
    };
    let digits = hex.as_bytes();
    let mut out = Vec::with_capacity(digits.len() / 2);
    let mut index = 0;
    while index + 1 < digits.len() {
        let high = (digits[index] as char).to_digit(16);
        let low = (digits[index + 1] as char).to_digit(16);
        match (high, low) {
            (Some(high), Some(low)) => out.push(((high << 4) | low) as u8),
            // 非法 hex（数据损坏）：截断保底，不 panic
            _ => break,
        }
        index += 2;
    }
    out
}

impl ChangeTracker {
    /// 修改前快照原始字节（None = 文件原本不存在）；已快照过的路径不重复读盘。
    pub fn snapshot(&mut self, path: &Path) -> Result<(), String> {
        if let std::collections::hash_map::Entry::Vacant(entry) =
            self.originals.entry(path.to_path_buf())
        {
            entry.insert(read_original_bytes(path)?);
            self.dirty.push(path.to_path_buf());
        }
        // 每轮口径：本轮首次写前同样记一笔（独立于会话级基线）
        if let std::collections::hash_map::Entry::Vacant(entry) =
            self.turn_originals.entry(path.to_path_buf())
        {
            entry.insert(read_original_bytes(path)?);
        }
        Ok(())
    }

    /// 取出新增快照路径（落盘后清空）
    pub fn take_dirty(&mut self) -> Vec<PathBuf> {
        std::mem::take(&mut self.dirty)
    }

    /// 读取某路径的原始快照（None 值 = 文件原本不存在；None 返回 = 未追踪）。
    /// 内部字节经 snapshot_to_store 转成持久化 String，session 侧 4MB 检查逻辑不变。
    pub fn original(&self, path: &Path) -> Option<Option<String>> {
        self.originals
            .get(path)
            .map(|original| original.as_deref().map(snapshot_to_store))
    }

    /// 重启后恢复基线（来自 file_originals 表；恢复的不标 dirty，避免回写）。
    /// 持久化 String 经 snapshot_from_store 还原为字节。
    pub fn restore(&mut self, entries: Vec<(PathBuf, Option<String>)>) {
        for (path, original) in entries {
            self.originals
                .insert(path, original.map(|s| snapshot_from_store(&s)));
        }
    }

    /// 生成「原始 → 当前」的 unified diff 与增删行数（两侧都先解码为 LF 视图）。
    pub fn diff(&mut self, cwd: &Path, path: &Path) -> Result<FileChange, String> {
        let original_bytes = self
            .originals
            .get(path)
            .ok_or_else(|| "文件未追踪".to_string())?
            .clone()
            .unwrap_or_default();
        let original = decoded_view(&original_bytes);
        let current_bytes =
            std::fs::read(path).map_err(|e| format!("读取失败 {}: {e}", path.display()))?;
        let current = decoded_view(&current_bytes);
        let diff = similar::TextDiff::from_lines(&original, &current);
        let mut additions = 0;
        let mut deletions = 0;
        for change in diff.iter_all_changes() {
            match change.tag() {
                similar::ChangeTag::Insert => additions += 1,
                similar::ChangeTag::Delete => deletions += 1,
                _ => {}
            }
        }
        self.stats
            .insert(path.to_path_buf(), (additions, deletions));
        let cwd_canonical = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
        let relative = path
            .strip_prefix(&cwd_canonical)
            .or_else(|_| path.strip_prefix(cwd))
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|_| path.to_path_buf());
        let relative = relative.to_string_lossy().replace('\\', "/");
        let unified = diff
            .unified_diff()
            .context_radius(3)
            .header(&format!("a/{relative}"), &format!("b/{relative}"))
            .to_string();
        Ok(FileChange {
            path: relative,
            unified_diff: unified,
            additions,
            deletions,
        })
    }

    /// 计算并清空「本轮改动」：每文件 本轮首次写前 → 当前磁盘 的净 diff（LF 视图）。
    /// 净额为零（本轮内改回原文）不产出；文件被删除的暂不产出。
    pub fn take_turn_changes(&mut self, cwd: &Path) -> Vec<FileChange> {
        let entries = std::mem::take(&mut self.turn_originals);
        let mut changes = Vec::new();
        for (path, before) in entries {
            let before = decoded_view(&before.unwrap_or_default());
            let Ok(current_bytes) = std::fs::read(&path) else {
                continue;
            };
            let current = decoded_view(&current_bytes);
            let change = per_edit_diff(cwd, &path, &before, &current);
            if change.additions == 0 && change.deletions == 0 {
                continue;
            }
            changes.push(change);
        }
        changes.sort_by(|a, b| a.path.cmp(&b.path));
        changes
    }

    /// 撤销：原始字节原样写回（GBK/UTF-16/CRLF 字节级精确）；新建文件删除。
    pub fn revert(&mut self, path: &Path) -> Result<(), String> {
        let Some(original) = self.originals.remove(path) else {
            return Err("文件未被修改过，无法撤销".to_string());
        };
        self.stats.remove(path);
        match original {
            Some(bytes) => {
                std::fs::write(path, bytes).map_err(|e| format!("恢复失败 {}: {e}", path.display()))
            }
            None => std::fs::remove_file(path)
                .map_err(|e| format!("删除新建文件失败 {}: {e}", path.display())),
        }
    }

    #[allow(dead_code)] // M4 会话统计会用
    pub fn totals(&self) -> (u32, u32) {
        self.stats
            .values()
            .fold((0, 0), |(a, d), (ta, td)| (a + ta, d + td))
    }

    pub fn tracked_paths(&self) -> Vec<String> {
        self.originals
            .keys()
            .map(|p| p.to_string_lossy().to_string())
            .collect()
    }
}

/// TodoList 工具的待办项：定义在 protocol（UI 面板共享），core 侧 re-export 兼容。
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

/// 审批判定：Plan 模式在更早处拦截（直接拒绝），这里只管其余档。
/// Yolo 与 FullAccess 都不审批（危险命令强制弹窗在 session 层，Yolo 在那里也跳过）。
pub fn requires_approval(tool: &dyn Tool, mode: ExecMode) -> bool {
    match mode {
        ExecMode::FullAccess | ExecMode::Yolo => false,
        ExecMode::Plan => false,
        ExecMode::ConfirmBeforeEdit => !tool.read_only(),
        ExecMode::AutoEdit => tool.is_shell(),
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
        "TaskList" => "列出后台任务".to_string(),
        "TaskOutput" | "TaskStop" => args["task_id"].as_str().unwrap_or("?").to_string(),
        "AskUserQuestion" => args["questions"][0]["question"]
            .as_str()
            .unwrap_or("?")
            .to_string(),
        "ExitPlanMode" => "请求退出计划模式".to_string(),
        "EnterPlanMode" => "请求进入计划模式".to_string(),
        _ => args.to_string(),
    };
    // 不在源头截断：折叠行由 UI 做单行省略，展开卡片要完整显示；
    // 全文本就在 arguments 里随 rollout 持久化，摘要不再额外截短
    raw
}

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
    let args: serde_json::Value = serde_json::from_str(&call.arguments).unwrap_or_default();
    let tools = all();
    let Some(tool) = tools.iter().find(|tool| tool.name() == call.name) else {
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

/// resolve_checked 的核心：纯路径解析 + 边界/符号链接检查，结构化返回越界形态。
fn resolve_core(cwd: &Path, path: &str, create_parents: bool) -> Result<PathBuf, BoundFailure> {
    let raw = Path::new(path);
    let full = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        cwd.join(raw)
    };
    let cwd_canonical = cwd
        .canonicalize()
        .map_err(|e| BoundFailure::Other(format!("工作目录无效 {}: {e}", cwd.display())))?;
    let parent = full.parent().unwrap_or(cwd);
    if create_parents {
        std::fs::create_dir_all(parent)
            .map_err(|e| BoundFailure::Other(format!("创建目录失败 {}: {e}", parent.display())))?;
    }
    let parent_canonical = parent
        .canonicalize()
        .map_err(|e| BoundFailure::Other(format!("目录不存在 {}: {e}", parent.display())))?;
    if !parent_canonical.starts_with(&cwd_canonical) {
        let file_name = full
            .file_name()
            .ok_or_else(|| BoundFailure::Other(format!("无效路径: {path}")))?;
        return Err(BoundFailure::ParentOutside {
            resolved: parent_canonical.join(file_name),
        });
    }
    let file_name = full
        .file_name()
        .ok_or_else(|| BoundFailure::Other(format!("无效路径: {path}")))?;
    let resolved = parent_canonical.join(file_name);
    let is_symlink = std::fs::symlink_metadata(&resolved)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false);
    if is_symlink || resolved.exists() {
        match resolved.canonicalize() {
            Ok(full_canonical) => {
                if !full_canonical.starts_with(&cwd_canonical) {
                    return Err(BoundFailure::TargetOutside { resolved });
                }
            }
            Err(_) if is_symlink => return Err(BoundFailure::DanglingSymlink),
            Err(e) => {
                return Err(BoundFailure::Other(format!(
                    "路径无效 {}: {e}",
                    resolved.display()
                )));
            }
        }
    }
    Ok(resolved)
}

/// 解析相对 cwd 的路径并防止越出工作目录。
/// 父目录 canonicalize 之外，目标本身也要校验符号链接：指向工作区外的拒绝；
/// 悬空链接（目标不存在，无法 canonicalize）fail-closed 拒绝——否则 Write 会
/// 顺着链接在工作区外创建文件。
pub fn resolve_checked(cwd: &Path, path: &str, create_parents: bool) -> Result<PathBuf, String> {
    resolve_core(cwd, path, create_parents).map_err(|failure| match failure {
        BoundFailure::ParentOutside { .. } => format!("路径越出工作目录: {path}"),
        BoundFailure::TargetOutside { .. } => format!("路径越出工作目录（符号链接）: {path}"),
        BoundFailure::DanglingSymlink => {
            format!("符号链接指向不存在的目标，无法确认安全性，已拒绝: {path}")
        }
        BoundFailure::Other(message) => message,
    })
}

/// 区外读/写策略：Read 看 fs_read_outside 开关，Write 看 fs_write_outside
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FsAccess {
    Read,
    Write,
}

/// 路径是否落在系统 tmp 下（macOS /tmp→/private/tmp，两边都 canonical 后比）。
/// 只对**绝对路径**请求生效（相对 ../ 逃逸落在 tmp 也视为越界，避免 tempdir
/// 工作区旁的目录成为后门）。
fn is_tmp_absolute_path(raw: &str) -> bool {
    let raw_path = Path::new(raw);
    if !raw_path.is_absolute() {
        return false;
    }
    // canonical 比较：请求路径可能不存在（Write 新文件），退化为父目录 canonical
    let candidate = raw_path
        .canonicalize()
        .or_else(|_| raw_path.parent().unwrap_or(raw_path).canonicalize())
        .unwrap_or_else(|_| raw_path.to_path_buf());
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Ok(tmp) = std::env::temp_dir().canonicalize() {
        roots.push(tmp);
    }
    if let Ok(tmp) = Path::new("/tmp").canonicalize() {
        roots.push(tmp);
    }
    roots.iter().any(|root| candidate.starts_with(root))
}

/// resolve_checked 的策略入口（文件工具统一走这里）：工作区内与显式 tmp 绝对路径
/// 永远放行；其余区外按会话开关；开关关时报错带模式菜单引导。悬空链接始终拒绝。
/// 敏感文件检查（is_sensitive_file）在 resolve 之后由工具无条件执行，不受开关影响。
pub fn resolve_with_access(
    state: &SessionToolState,
    cwd: &Path,
    path: &str,
    create_parents: bool,
    access: FsAccess,
) -> Result<PathBuf, String> {
    match resolve_core(cwd, path, create_parents) {
        Ok(resolved) => Ok(resolved),
        Err(BoundFailure::Other(message)) => Err(message),
        Err(BoundFailure::DanglingSymlink) => Err(format!(
            "符号链接指向不存在的目标，无法确认安全性，已拒绝: {path}"
        )),
        Err(
            BoundFailure::ParentOutside { resolved } | BoundFailure::TargetOutside { resolved },
        ) => {
            if is_tmp_absolute_path(path) {
                return Ok(resolved);
            }
            let allowed = match access {
                FsAccess::Read => state.fs_read_outside.load(Ordering::Relaxed),
                FsAccess::Write => state.fs_write_outside.load(Ordering::Relaxed),
            };
            if allowed {
                // 同一个闸：开关开时指向区外的 symlink 也放行
                Ok(resolved)
            } else {
                let toggle = match access {
                    FsAccess::Read => "允许读取工作区外文件",
                    FsAccess::Write => "允许写入工作区外文件",
                };
                Err(format!(
                    "路径越出工作目录: {path}（可在输入框模式菜单开启「{toggle}」）"
                ))
            }
        }
    }
}

/// 敏感文件判定（大小写不敏感，只看文件名）：.env 家族 / SSH 私钥 / 云凭据。
pub fn is_sensitive_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let lower = name.to_ascii_lowercase();
    // .env 与 .env.*（模板类豁免）
    if (lower == ".env" || lower.starts_with(".env."))
        && !matches!(
            lower.as_str(),
            ".env.example" | ".env.sample" | ".env.template"
        )
    {
        return true;
    }
    // SSH 私钥：精确名或 id_xxx[-_.] 变体；.pub 公钥一律豁免
    if !lower.ends_with(".pub") {
        for prefix in ["id_rsa", "id_ed25519", "id_ecdsa", "id_dsa"] {
            if lower == prefix {
                return true;
            }
            if let Some(rest) = lower.strip_prefix(prefix) {
                if rest
                    .chars()
                    .next()
                    .is_some_and(|c| matches!(c, '-' | '_' | '.'))
                {
                    return true;
                }
            }
        }
    }
    // 云凭据：~/.aws/credentials、~/.gcp/credentials
    if lower == "credentials" {
        let parent = path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .map(str::to_ascii_lowercase);
        if parent.as_deref() == Some(".aws") || parent.as_deref() == Some(".gcp") {
            return true;
        }
    }
    false
}

/// 敏感文件拒绝文案（Read/Write/Edit 共用）
fn sensitive_file_error(path: &Path) -> String {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.display().to_string());
    format!("已拒绝访问敏感文件: {name}（.env / 私钥 / 云凭据不会进入模型上下文）")
}

struct ReadFile;
struct WriteFile;
struct EditFile;
struct Glob;
struct Grep;
struct Bash;

impl Tool for ReadFile {
    fn name(&self) -> &'static str {
        "Read"
    }

    fn read_only(&self) -> bool {
        true
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "Read",
                "description": "读取工作区内文件内容，输出带「行号\\t」前缀。path 相对工作目录；文件过长时用 offset/limit 分页（单次约 10 万字符上限，单行超 2000 字符会截断）。UTF-16/GBK 文件自动转码显示，二进制文件会拒绝。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "相对工作目录的文件路径" },
                        "offset": { "type": "integer", "description": "起始行号（从 1 开始），默认 1" },
                        "limit": { "type": "integer", "description": "最多读取行数，默认 2000" }
                    },
                    "required": ["path"]
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
            let path = args["path"].as_str().ok_or("缺少参数 path")?;
            let full = resolve_with_access(ctx.state, ctx.cwd, path, false, FsAccess::Read)?;
            if is_sensitive_file(&full) {
                return Err(sensitive_file_error(&full));
            }
            let bytes = std::fs::read(&full).map_err(|e| read_io_error(path, &full, e))?;
            let doc = match crate::text::decode(&bytes) {
                Ok(doc) => doc,
                Err(error) => {
                    // 图片给明确指引（魔数嗅探，不信任扩展名）
                    if let Some(mime) = sniff_image(&bytes) {
                        let label = match mime {
                            "image/png" => "PNG",
                            "image/jpeg" => "JPEG",
                            "image/gif" => "GIF",
                            "image/webp" => "WebP",
                            _ => mime,
                        };
                        return Err(format!(
                            "这是 {label} 图片，请改用 ReadMediaFile 读取（当前模型需支持图片输入）"
                        ));
                    }
                    return Err(error);
                }
            };
            if doc.text.is_empty() {
                record_read_state(ctx.state, &full, &bytes, false);
                return Ok(ToolEffect::plain("（空文件）".to_string()));
            }
            let offset = args["offset"].as_u64().unwrap_or(1).max(1) as usize;
            let limit = args["limit"].as_u64().unwrap_or(MAX_READ_LINES as u64) as usize;
            let lines: Vec<&str> = doc.text.lines().collect();
            let total = lines.len();
            let start = (offset - 1).min(total);
            // 逐行渲染（带行号），行数上限与字符预算（含行号前缀）先到先停
            let mut rendered: Vec<String> = Vec::new();
            let mut used_chars = 0usize;
            let mut end = start;
            for (index, line) in lines.iter().enumerate().skip(start) {
                if index - start >= limit {
                    break;
                }
                let line_no = index + 1;
                let line_chars = line.chars().count();
                let body = if line_chars > MAX_LINE_CHARS {
                    let taken: String = line.chars().take(MAX_LINE_CHARS).collect();
                    format!("{taken} [...本行已截断，共 {line_chars} 字符]")
                } else {
                    (*line).to_string()
                };
                let row = format!("{line_no}\t{body}");
                let row_chars = row.chars().count();
                if !rendered.is_empty() && used_chars + row_chars > MAX_READ_CHARS {
                    break;
                }
                used_chars += row_chars;
                rendered.push(row);
                end = index + 1;
            }
            let mut out = rendered.join("\n");
            if end < total {
                out.push_str(&format!(
                    "\n\n[已截断: 显示 {}-{end} 行，共 {total} 行；用 offset 参数继续读取]",
                    start + 1
                ));
            }
            // 元信息：仅非默认编码/行尾或 lossy 时提示
            if doc.encoding != FileEncoding::Utf8 || doc.line_ending == LineEnding::Crlf {
                let mut parts = vec![format!("编码={}", doc.encoding.label())];
                if doc.line_ending == LineEnding::Crlf {
                    parts.push("行尾=CRLF（已转为 LF 显示，写回时还原）".to_string());
                }
                out.push_str(&format!("\n\n[文件信息: {}]", parts.join(", ")));
            }
            if doc.lossy {
                out.push_str("\n[警告: 解码存在替换字符，编码识别可能有误]");
            }
            // ZCode 口径：只有被预算截断的「整读」才算 partial；显式分页读不算
            let paged = args.get("offset").is_some() || args.get("limit").is_some();
            record_read_state(ctx.state, &full, &bytes, !paged && end < total);
            Ok(ToolEffect::plain(out))
        })
    }
}

/// Read/Edit 共用的读盘报错：文件不存在时附父目录下最多 20 个文件名，引导模型修正路径
fn read_io_error(path: &str, full: &Path, error: std::io::Error) -> String {
    if error.kind() == std::io::ErrorKind::NotFound {
        let mut message = format!("文件不存在: {path}");
        if let Some(parent) = full.parent() {
            if let Ok(entries) = std::fs::read_dir(parent) {
                let mut names: Vec<String> = entries
                    .flatten()
                    .map(|entry| entry.file_name().to_string_lossy().to_string())
                    .collect();
                names.sort();
                names.truncate(20);
                if !names.is_empty() {
                    message.push_str(&format!(
                        "。目录 {} 下有: {}",
                        parent.display(),
                        names.join(", ")
                    ));
                }
            }
        }
        message
    } else {
        format!("读取失败 {}: {error}", full.display())
    }
}

/// 文件内容指纹（原始字节的 DefaultHasher）
fn hash_bytes(bytes: &[u8]) -> u64 {
    use std::hash::Hasher as _;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash_slice(bytes, &mut hasher);
    hasher.finish()
}

/// Read 成功登记 / Write/Edit 写盘后刷新 新鲜度状态（mtime 取写盘后的新值）
fn record_read_state(state: &SessionToolState, full: &Path, bytes: &[u8], partial: bool) {
    let mtime = std::fs::metadata(full).ok().and_then(|m| m.modified().ok());
    let entry = crate::task::ReadState {
        mtime,
        size: bytes.len() as u64,
        hash: hash_bytes(bytes),
        partial,
    };
    if let Ok(mut states) = state.read_states.lock() {
        states.insert(full.to_path_buf(), entry);
    }
}

/// 写前新鲜度检查（ZCode read-file-state 同款）：文件不存在（新建）放行；
/// 未读过 / 上次是不完整视图 / 读后磁盘被外部改动，一律拒绝。
/// mtime 或 size 有变化才比 hash；hash 相同（内容逐字未变）放行并顺手更新状态。
fn check_fresh(
    state: &SessionToolState,
    full: &Path,
    file_exists: bool,
    verb: &str,
) -> Result<(), String> {
    if !file_exists {
        return Ok(());
    }
    let (read_mtime, read_size, read_hash, partial) = {
        let states = state.read_states.lock().map_err(|e| e.to_string())?;
        let Some(read) = states.get(full) else {
            return Err(format!(
                "文件已存在且本会话尚未读过；为避免覆盖他人改动，请先 Read 再{verb}"
            ));
        };
        (read.mtime, read.size, read.hash, read.partial)
    };
    if partial {
        return Err(
            "上次 Read 是不完整视图（输出被截断）；请用 offset/limit 分页读完或完整 Read 后再改"
                .to_string(),
        );
    }
    let meta =
        std::fs::metadata(full).map_err(|e| format!("读取文件状态失败 {}: {e}", full.display()))?;
    if meta.modified().ok() == read_mtime && meta.len() == read_size {
        return Ok(());
    }
    let bytes = std::fs::read(full).map_err(|e| format!("读取失败 {}: {e}", full.display()))?;
    if hash_bytes(&bytes) == read_hash {
        record_read_state(state, full, &bytes, false);
        return Ok(());
    }
    Err("文件自上次 Read 后已被外部修改，请先重新 Read 再改（避免覆盖他人改动）".to_string())
}

impl Tool for WriteFile {
    fn name(&self) -> &'static str {
        "Write"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "Write",
                "description": "写入整个文件（自动创建父目录）。大文件优先用 Edit 做局部修改。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "相对工作目录的文件路径" },
                        "content": { "type": "string", "description": "完整文件内容" }
                    },
                    "required": ["path", "content"]
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
            let path = args["path"].as_str().ok_or("缺少参数 path")?;
            let content = args["content"].as_str().ok_or("缺少参数 content")?;
            let full = resolve_with_access(ctx.state, ctx.cwd, path, true, FsAccess::Write)?;
            if is_sensitive_file(&full) {
                return Err(sensitive_file_error(&full));
            }
            // 已存在文件沿用其编码/BOM/行尾写回；无法识别（二进制/未知编码）按 UTF-8/LF 覆盖；
            // 新文件一律 UTF-8/LF
            let existing = std::fs::read(&full).ok();
            check_fresh(ctx.state, &full, existing.is_some(), "写入")?;
            let (encoding, bom, line_ending, note) = match &existing {
                Some(bytes) => match crate::text::decode(bytes) {
                    Ok(doc) => {
                        let note = match (
                            doc.encoding != FileEncoding::Utf8,
                            doc.line_ending == LineEnding::Crlf,
                        ) {
                            (true, true) => {
                                format!("（保留原编码 {} / CRLF）", doc.encoding.label())
                            }
                            (true, false) => format!("（保留原编码 {}）", doc.encoding.label()),
                            (false, true) => "（保留原行尾 CRLF）".to_string(),
                            (false, false) => String::new(),
                        };
                        (doc.encoding, doc.bom, doc.line_ending, note)
                    }
                    Err(_) => (
                        FileEncoding::Utf8,
                        false,
                        LineEnding::Lf,
                        "（原文件编码无法识别，已按 UTF-8 覆盖）".to_string(),
                    ),
                },
                None => (FileEncoding::Utf8, false, LineEnding::Lf, String::new()),
            };
            // diff 两侧都用 LF 视图：before = 原文件解码文本（或 ""），after = 归一后的 LF 文本
            let before = existing.as_deref().map(decoded_view).unwrap_or_default();
            let after = content.replace("\r\n", "\n");
            ctx.tracker.snapshot(&full)?;
            let bytes = crate::text::encode(content, encoding, bom, line_ending)?;
            std::fs::write(&full, &bytes)
                .map_err(|e| format!("写入失败 {}: {e}", full.display()))?;
            // 写盘后刷新新鲜度：紧接着再 Edit 自己刚写的文件必须合法
            record_read_state(ctx.state, &full, &bytes, false);
            let file_change = ctx.tracker.diff(ctx.cwd, &full).ok();
            let edit_diff = Some(per_edit_diff(ctx.cwd, &full, &before, &after));
            Ok(ToolEffect {
                output: format!("已写入 {}（{} 字节）{note}", path, bytes.len()),
                file_change,
                edit_diff,
                images: vec![],
            })
        })
    }
}

impl Tool for EditFile {
    fn name(&self) -> &'static str {
        "Edit"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "Edit",
                "description": "精确替换文件中的文本。old_string 必须在文件中唯一出现（replace_all=true 时替换全部出现）；先 Read 确认内容再改。保留原文件的编码与行尾。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "相对工作目录的文件路径" },
                        "old_string": { "type": "string", "description": "要被替换的原文（须唯一出现；replace_all=true 时替换全部）" },
                        "new_string": { "type": "string", "description": "替换后的新文本；为空且 old_string 占整行时连行尾换行一起删除，不留空行" },
                        "replace_all": { "type": "boolean", "description": "true 时替换全部匹配（默认 false，要求唯一出现）" }
                    },
                    "required": ["path", "old_string", "new_string"]
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
            let path = args["path"].as_str().ok_or("缺少参数 path")?;
            let old = args["old_string"].as_str().ok_or("缺少参数 old_string")?;
            let new = args["new_string"].as_str().ok_or("缺少参数 new_string")?;
            if old.is_empty() {
                return Err("old_string 不能为空；要创建文件请用 Write".to_string());
            }
            if old == new {
                return Err("old_string 与 new_string 相同，无需修改".to_string());
            }
            let replace_all = args["replace_all"].as_bool().unwrap_or(false);
            let full = resolve_with_access(ctx.state, ctx.cwd, path, false, FsAccess::Write)?;
            if is_sensitive_file(&full) {
                return Err(sensitive_file_error(&full));
            }
            check_fresh(ctx.state, &full, full.exists(), "修改")?;
            let bytes = std::fs::read(&full).map_err(|e| read_io_error(path, &full, e))?;
            let doc = crate::text::decode(&bytes)?;
            let content = doc.text;
            // 匹配+替换计算抽在 compute_edit（审批预览共用，预览即所得）
            let outcome = match compute_edit(&content, old, new, replace_all) {
                Ok(outcome) => outcome,
                Err(EditMatchError::NotFound) => {
                    let mut message = format!(
                        "old_string 在 {path} 中未找到。请先用 Read 确认文件当前内容（注意缩进与换行需完全一致）。"
                    );
                    if doc.line_ending == LineEnding::Crlf {
                        message.push_str(
                            "该文件为 CRLF 行尾，Read 输出已转为 LF，old_string 请用 LF 换行。",
                        );
                    }
                    return Err(message);
                }
                Err(EditMatchError::NotUnique { count }) => {
                    return Err(format!(
                        "old_string 在 {path} 中出现 {count} 次，无法唯一定位。请扩大 old_string 范围使其唯一；如需全部替换，设 replace_all=true。"
                    ));
                }
            };
            ctx.tracker.snapshot(&full)?;
            // 匹配与替换都在 LF 视图（解码已归一）上做；写回时还原原编码/行尾
            let after = outcome.after;
            let encoded = crate::text::encode(&after, doc.encoding, doc.bom, doc.line_ending)?;
            std::fs::write(&full, &encoded)
                .map_err(|e| format!("写入失败 {}: {e}", full.display()))?;
            // 写盘后刷新新鲜度：紧接着再改自己刚写的文件必须合法
            record_read_state(ctx.state, &full, &encoded, false);
            let file_change = ctx.tracker.diff(ctx.cwd, &full).ok();
            let edit_diff = Some(per_edit_diff(ctx.cwd, &full, &content, &after));
            let output = if replace_all {
                format!("已修改 {path}（替换 {} 处）", outcome.replaced)
            } else if let Some(note) = outcome.tier_note {
                format!("已修改 {path}（{note}）")
            } else {
                format!("已修改 {path}")
            };
            Ok(ToolEffect {
                output,
                file_change,
                edit_diff,
                images: vec![],
            })
        })
    }
}

/// Edit 匹配失败的两种形态（报错文案在调用方拼，那里才有 path/行尾上下文）
#[derive(Debug)]
pub enum EditMatchError {
    NotFound,
    NotUnique { count: usize },
}

/// compute_edit 的产物：替换后文本、替换处数、容错梯队命中说明
pub struct EditOutcome {
    pub after: String,
    pub replaced: usize,
    /// 「容错匹配：已剥离行号前缀」/「容错匹配：引号风格已跟随文件」；精确命中为 None
    pub tier_note: Option<&'static str>,
}

/// Edit 的匹配+替换计算（LF 视图）：精确 → 剥离 Read 行号前缀 → 引号归一 三级梯队，
/// 每级各自做唯一性检查；replace_all 只走精确（宽匹配仅做单次替换）。
/// 审批预览与 Edit 执行共用，保证「预览即所得」。
pub fn compute_edit(
    content_lf: &str,
    old: &str,
    new: &str,
    replace_all: bool,
) -> Result<EditOutcome, EditMatchError> {
    let mut effective_old = old.to_string();
    let mut effective_new = new.to_string();
    let mut quote_window: Option<(usize, usize)> = None;
    let mut tier_note: Option<&'static str> = None;
    let mut count = content_lf.matches(old).count();
    if count == 0 && !replace_all {
        if let Some(stripped) = strip_line_number_prefixes(old) {
            let stripped_count = content_lf.matches(&stripped).count();
            if stripped_count > 0 {
                count = stripped_count;
                effective_old = stripped;
                tier_note = Some("容错匹配：已剥离行号前缀");
            }
        }
        if count == 0 {
            let windows = find_quote_normalized_windows(content_lf, old);
            if windows.len() == 1 {
                let (start, end) = windows[0];
                count = 1;
                quote_window = Some((start, end));
                effective_new = follow_quote_style(new, &content_lf[start..end]);
                tier_note = Some("容错匹配：引号风格已跟随文件");
            } else if windows.len() > 1 {
                count = windows.len();
            }
        }
    }
    if count == 0 {
        return Err(EditMatchError::NotFound);
    }
    if count > 1 && !replace_all {
        return Err(EditMatchError::NotUnique { count });
    }
    let (after, replaced) = match quote_window {
        // 引号归一命中：整段替换原文那 N 行
        Some((start, end)) => {
            let mut out = String::with_capacity(content_lf.len());
            out.push_str(&content_lf[..start]);
            out.push_str(&effective_new);
            out.push_str(&content_lf[end..]);
            (out, 1)
        }
        None => apply_replacement(content_lf, &effective_old, &effective_new, replace_all),
    };
    Ok(EditOutcome {
        after,
        replaced,
        tier_note,
    })
}

/// 审批授权的粒度（「本会话内始终允许」记忆键）：
/// Bash → 命令首词（二进制名）；Write/Edit → path；其余工具 → 空串（工具级）。
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

/// 保守只读命令判定（AutoEdit 直通用，宁漏不放）：单条简单命令——
/// 无管道/重定向/链式/命令替换/多行，且首词在白名单；
/// git 再看子命令白名单（branch/remote/tag 仅无参列表形态）。
pub fn is_readonly_command(command: &str) -> bool {
    if command
        .chars()
        .any(|c| matches!(c, '>' | '<' | '|' | '&' | ';' | '`' | '\n' | '\r'))
        || command.contains("$(")
    {
        return false;
    }
    let tokens: Vec<&str> = command.split_whitespace().collect();
    let Some(&first) = tokens.first() else {
        return false;
    };
    const READONLY: &[&str] = &[
        "ls", "cat", "head", "tail", "pwd", "echo", "find", "grep", "rg", "wc", "file", "stat",
        "which", "whoami", "date", "uname", "hostname", "tree", "du", "df", "sort", "uniq", "diff",
    ];
    if READONLY.contains(&first) {
        return true;
    }
    if first == "git" {
        let second = tokens.get(1).copied().unwrap_or("");
        const GIT_READONLY: &[&str] = &[
            "status",
            "log",
            "diff",
            "show",
            "rev-parse",
            "ls-files",
            "blame",
            "describe",
            "shortlog",
        ];
        if GIT_READONLY.contains(&second) {
            return true;
        }
        // branch/remote/tag 仅纯列表形态（无第三个参数）
        if matches!(second, "branch" | "remote" | "tag") && tokens.len() == 2 {
            return true;
        }
    }
    false
}

/// 手动扫描替换（不用 String::replace，实现 ZCode 同款删除优化）：
/// new 为空、old 不以 \n 结尾、且匹配位置后紧跟 \n 时，连这个 \n 一起删，不留空行。
/// 返回 (替换后文本, 替换处数)。
fn apply_replacement(content: &str, old: &str, new: &str, replace_all: bool) -> (String, usize) {
    let mut out = String::with_capacity(content.len());
    let mut rest = content;
    let mut replaced = 0usize;
    while let Some(pos) = rest.find(old) {
        out.push_str(&rest[..pos]);
        out.push_str(new);
        rest = &rest[pos + old.len()..];
        if new.is_empty() && !old.ends_with('\n') && rest.starts_with('\n') {
            rest = &rest[1..];
        }
        replaced += 1;
        if !replace_all {
            break;
        }
    }
    out.push_str(rest);
    (out, replaced)
}

/// Edit 容错第 2 级：剥离 Read 输出的行号前缀（每行 ^\d+\t 或 ^\d+:）。
/// 任一行不带合法前缀则整体不剥离（返回 None）；行尾单个 \n 视为终止符。
fn strip_line_number_prefixes(s: &str) -> Option<String> {
    let (body, trailing_newline) = match s.strip_suffix('\n') {
        Some(body) => (body, true),
        None => (s, false),
    };
    let mut lines = Vec::new();
    for line in body.split('\n') {
        let digit_len = line.bytes().take_while(|b| b.is_ascii_digit()).count();
        let rest = &line[digit_len..];
        let stripped = if digit_len > 0 && rest.starts_with('\t') {
            &rest[1..]
        } else if digit_len > 0 && rest.starts_with(':') {
            &rest[1..]
        } else {
            return None;
        };
        lines.push(stripped);
    }
    let joined = lines.join("\n");
    if joined.is_empty() {
        return None;
    }
    Some(if trailing_newline {
        format!("{joined}\n")
    } else {
        joined
    })
}

/// Edit 容错第 3 级的弯引号归一：‘’→'，“”→"。
fn normalize_quotes(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\u{2018}' | '\u{2019}' => '\'',
            '\u{201C}' | '\u{201D}' => '"',
            _ => c,
        })
        .collect()
}

/// 引号归一后按行窗口匹配：content 与 old 各按行归一，逐行相等即命中。
/// 返回各命中窗口在 content 里的字节区间（窗口覆盖整 N 行，不含尾随换行）。
/// old 行尾单个 \n 视为终止符。
fn find_quote_normalized_windows(content: &str, old: &str) -> Vec<(usize, usize)> {
    let old_body = old.strip_suffix('\n').unwrap_or(old);
    let old_lines: Vec<String> = old_body.split('\n').map(normalize_quotes).collect();
    let n = old_lines.len();
    let content_lines: Vec<&str> = content.split('\n').collect();
    if n == 0 || content_lines.len() < n {
        return Vec::new();
    }
    // 每行的字节起始偏移（split('\n') 的分隔符恰为 1 字节）
    let mut offsets = Vec::with_capacity(content_lines.len());
    let mut pos = 0usize;
    for line in &content_lines {
        offsets.push(pos);
        pos += line.len() + 1;
    }
    let normalized: Vec<String> = content_lines
        .iter()
        .map(|line| normalize_quotes(line))
        .collect();
    let mut hits = Vec::new();
    for i in 0..=(content_lines.len() - n) {
        if (0..n).all(|j| normalized[i + j] == old_lines[j]) {
            let start = offsets[i];
            let end = offsets[i + n - 1] + content_lines[i + n - 1].len();
            hits.push((start, end));
        }
    }
    hits
}

/// 跟随文件引号风格：原文匹配段含弯引号时，把 new_string 的直引号按出现顺序交替转弯。
fn follow_quote_style(new_string: &str, original_segment: &str) -> String {
    let curly_double =
        original_segment.contains('\u{201C}') || original_segment.contains('\u{201D}');
    let curly_single =
        original_segment.contains('\u{2018}') || original_segment.contains('\u{2019}');
    if !curly_double && !curly_single {
        return new_string.to_string();
    }
    let mut out = String::with_capacity(new_string.len());
    let mut double_open = true;
    let mut single_open = true;
    for c in new_string.chars() {
        match c {
            '"' if curly_double => {
                out.push(if double_open { '\u{201C}' } else { '\u{201D}' });
                double_open = !double_open;
            }
            '\'' if curly_single => {
                out.push(if single_open { '\u{2018}' } else { '\u{2019}' });
                single_open = !single_open;
            }
            _ => out.push(c),
        }
    }
    out
}

/// 图片魔数嗅探（读文件头，不信任扩展名）：命中返回 mime，否则 None。
/// Read/ReadMediaFile 共用。
pub fn sniff_image(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// 手写 base64（标准 alphabet + `=` 填充；不加依赖，与快照 hex 编码同风格）
pub fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((n >> 18) & 63) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// 媒体文件的尺寸/体积上限（ReadMediaFile 与粘贴发送共用）
const MAX_MEDIA_FILE_BYTES: u64 = 100 * 1024 * 1024;
const MAX_MEDIA_PIXELS: u64 = 100_000_000;
/// 默认缩放到最长边 2000（full_resolution=true 时不缩）
const MEDIA_MAX_EDGE: u32 = 2000;
/// PNG 输出超过 4MB 且无 alpha → 转 JPEG q85 兜底
const MAX_PNG_BYTES: usize = 4 * 1024 * 1024;

/// 压缩产物（进模型预算的图片）
pub struct CompressedImage {
    pub bytes: Vec<u8>,
    pub media_type: String,
    pub width: u32,
    pub height: u32,
}

/// 解码后图片的预算内编码：最长边 2000 等比缩放（小的不动），
/// 有 alpha 或源是 PNG/GIF/WebP → PNG；否则 JPEG q85；PNG 超 4MB 无 alpha → JPEG 兜底。
/// ReadMediaFile（region 裁剪后）与粘贴发送共用这一段。
pub fn encode_image_for_model(
    mut image: image::DynamicImage,
    source_mime: &str,
) -> Result<CompressedImage, String> {
    let (mut w, mut h) = (image.width(), image.height());
    if w.max(h) > MEDIA_MAX_EDGE {
        let scale = MEDIA_MAX_EDGE as f32 / w.max(h) as f32;
        let (nw, nh) = (
            (w as f32 * scale).round().max(1.) as u32,
            (h as f32 * scale).round().max(1.) as u32,
        );
        image = image.resize(nw, nh, image::imageops::FilterType::Triangle);
        w = nw;
        h = nh;
    }
    if w as u64 * h as u64 > MAX_MEDIA_PIXELS {
        return Err(format!("图片过大（{w}×{h}），请用 region 参数裁剪局部"));
    }
    let has_alpha = image.color().has_alpha();
    let prefer_png = has_alpha || matches!(source_mime, "image/png" | "image/gif" | "image/webp");
    let mut media_type = if prefer_png {
        "image/png"
    } else {
        "image/jpeg"
    };
    let mut encoded = if prefer_png {
        let mut buf = std::io::Cursor::new(Vec::new());
        image
            .write_to(&mut buf, image::ImageFormat::Png)
            .map_err(|e| format!("图片编码失败: {e}"))?;
        buf.into_inner()
    } else {
        encode_jpeg(&image)?
    };
    if prefer_png && encoded.len() > MAX_PNG_BYTES && !has_alpha {
        encoded = encode_jpeg(&image)?;
        media_type = "image/jpeg";
    }
    Ok(CompressedImage {
        bytes: encoded,
        media_type: media_type.to_string(),
        width: w,
        height: h,
    })
}

/// 原始字节 → 压缩产物（粘贴发送路径）：source_mime 为空串时魔数嗅探。
/// 尺寸预检（不解码大图）→ 解码 → encode_image_for_model。
pub fn compress_image_for_model(
    bytes: &[u8],
    source_mime: &str,
) -> Result<CompressedImage, String> {
    let sniffed = if source_mime.is_empty() {
        sniff_image(bytes).ok_or("不是可识别的图片（支持 PNG/JPEG/GIF/WebP）".to_string())?
    } else {
        source_mime
    };
    let reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| format!("图片解析失败: {e}"))?;
    let (w, h) = reader
        .into_dimensions()
        .map_err(|e| format!("图片解析失败: {e}"))?;
    if w as u64 * h as u64 > MAX_MEDIA_PIXELS {
        return Err(format!("图片过大（{w}×{h}）"));
    }
    let image = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| format!("图片解析失败: {e}"))?
        .decode()
        .map_err(|e| format!("图片解码失败: {e}"))?;
    encode_image_for_model(image, sniffed)
}

struct ReadMediaFile;

impl Tool for ReadMediaFile {
    fn name(&self) -> &'static str {
        "ReadMediaFile"
    }

    fn read_only(&self) -> bool {
        true
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "ReadMediaFile",
                "description": "读取图片文件进上下文（PNG/JPEG/GIF/WebP，魔数嗅探不信任扩展名）。默认等比缩放到最长边 2000 像素；region 可按原图坐标裁剪局部，full_resolution=true 不缩放。需要模型支持图片输入。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "相对工作目录的图片路径" },
                        "region": {
                            "type": "object",
                            "description": "可选裁剪区域（原图像素坐标；越界自动夹紧，不相交报错）",
                            "properties": {
                                "x": { "type": "integer" },
                                "y": { "type": "integer" },
                                "width": { "type": "integer" },
                                "height": { "type": "integer" }
                            },
                            "required": ["x", "y", "width", "height"]
                        },
                        "full_resolution": { "type": "boolean", "description": "true 时不做 2000px 缩放（默认 false）" }
                    },
                    "required": ["path"]
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
            let path = args["path"].as_str().ok_or("缺少参数 path")?;
            let full = resolve_with_access(ctx.state, ctx.cwd, path, false, FsAccess::Read)?;
            if is_sensitive_file(&full) {
                return Err(sensitive_file_error(&full));
            }
            let bytes = std::fs::read(&full).map_err(|e| read_io_error(path, &full, e))?;
            if bytes.len() as u64 > MAX_MEDIA_FILE_BYTES {
                return Err(format!(
                    "文件超过 100MB 上限（{}MB）",
                    bytes.len() / 1024 / 1024
                ));
            }
            let Some(source_mime) = sniff_image(&bytes) else {
                return Err("不是可识别的图片（支持 PNG/JPEG/GIF/WebP）；视频暂不支持".to_string());
            };
            // 先读尺寸再解码：总像素超限直接拒（防解码大图撑爆内存）
            let reader = image::ImageReader::new(std::io::Cursor::new(&bytes))
                .with_guessed_format()
                .map_err(|e| format!("图片解析失败: {e}"))?;
            let (orig_w, orig_h) = reader
                .into_dimensions()
                .map_err(|e| format!("图片解析失败: {e}"))?;
            if orig_w as u64 * orig_h as u64 > MAX_MEDIA_PIXELS {
                return Err(format!(
                    "图片过大（{orig_w}×{orig_h}），请用 region 参数裁剪局部"
                ));
            }
            let image = image::ImageReader::new(std::io::Cursor::new(&bytes))
                .with_guessed_format()
                .map_err(|e| format!("图片解析失败: {e}"))?
                .decode()
                .map_err(|e| format!("图片解码失败: {e}"))?;

            // region 裁剪（原图坐标；夹紧到图内，完全不相交报错）
            let mut crop_note = String::new();
            let image = if let Some(region) = args.get("region") {
                let (rx, ry, rw, rh) = (
                    region["x"].as_u64().unwrap_or(0),
                    region["y"].as_u64().unwrap_or(0),
                    region["width"].as_u64().unwrap_or(0),
                    region["height"].as_u64().unwrap_or(0),
                );
                let (ix, iy) = (rx.min(orig_w as u64) as u32, ry.min(orig_h as u64) as u32);
                let (ix2, iy2) = (
                    (rx + rw).min(orig_w as u64) as u32,
                    (ry + rh).min(orig_h as u64) as u32,
                );
                if ix >= ix2 || iy >= iy2 {
                    return Err(format!(
                        "裁剪区域（x={rx}, y={ry}, {rw}×{rh}）与图片（{orig_w}×{orig_h}）不相交"
                    ));
                }
                crop_note = format!("，裁剪 ({rx},{ry})→({ix2},{iy2})");
                image.crop_imm(ix, iy, ix2 - ix, iy2 - iy)
            } else {
                image
            };

            // 默认等比缩到最长边 2000；full_resolution 不缩（共享编码管线恒定缩放，
            // 故 full_resolution 走独立分支：只编码不缩放）
            let full_resolution = args["full_resolution"].as_bool().unwrap_or(false);
            let compressed = if full_resolution {
                let has_alpha = image.color().has_alpha();
                let prefer_png =
                    has_alpha || matches!(source_mime, "image/png" | "image/gif" | "image/webp");
                let w = image.width();
                let h = image.height();
                let mut media_type = if prefer_png {
                    "image/png"
                } else {
                    "image/jpeg"
                };
                let mut encoded = if prefer_png {
                    let mut buf = std::io::Cursor::new(Vec::new());
                    image
                        .write_to(&mut buf, image::ImageFormat::Png)
                        .map_err(|e| format!("图片编码失败: {e}"))?;
                    buf.into_inner()
                } else {
                    encode_jpeg(&image)?
                };
                if prefer_png && encoded.len() > MAX_PNG_BYTES && !has_alpha {
                    encoded = encode_jpeg(&image)?;
                    media_type = "image/jpeg";
                }
                CompressedImage {
                    bytes: encoded,
                    media_type: media_type.to_string(),
                    width: w,
                    height: h,
                }
            } else {
                encode_image_for_model(image, source_mime)?
            };
            let (w, h) = (compressed.width, compressed.height);
            let kb = compressed.bytes.len() / 1024;
            Ok(ToolEffect {
                output: format!(
                    "已读取图片 {path}（原始 {orig_w}×{orig_h}{crop_note} → 输出 {w}×{h}，{}，{kb}KB）",
                    compressed.media_type
                ),
                file_change: None,
                edit_diff: None,
                images: vec![ToolImage {
                    media_type: compressed.media_type,
                    data_base64: base64_encode(&compressed.bytes),
                    width: w,
                    height: h,
                }],
            })
        })
    }
}

/// 只读尺寸（不解码）；非图片返回 None。粘贴 chip 展示用。
pub fn image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

/// JPEG q85 编码（转 RGB 丢弃 alpha）
fn encode_jpeg(image: &image::DynamicImage) -> Result<Vec<u8>, String> {
    let mut buf = std::io::Cursor::new(Vec::new());
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 85)
        .encode_image(&image.to_rgb8())
        .map_err(|e| format!("图片编码失败: {e}"))?;
    Ok(buf.into_inner())
}

impl Tool for Glob {
    fn name(&self) -> &'static str {
        "Glob"
    }

    fn read_only(&self) -> bool {
        true
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "Glob",
                "description": "按文件名模式匹配工作区文件（如 **/*.rs）。尊重 .gitignore/.ignore，包含隐藏文件，按最近修改排序；敏感文件（.env/私钥等）自动过滤。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "description": "glob 模式；含 / 时按相对路径匹配，不含 / 时只匹配文件名" },
                        "path": { "type": "string", "description": "搜索根目录（相对工作目录），默认为工作目录" }
                    },
                    "required": ["pattern"]
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
            let pattern = args["pattern"].as_str().ok_or("缺少参数 pattern")?;
            let glob_pattern = glob::Pattern::new(pattern)
                .map_err(|e| format!("无效 glob 模式 {pattern}: {e}"))?;
            let root = match args["path"].as_str() {
                Some(path) => resolve_with_access(ctx.state, ctx.cwd, path, false, FsAccess::Read)?,
                None => ctx.cwd.to_path_buf(),
            };
            let mut files = walk_workspace(&root);
            sort_by_mtime_desc(&mut files);
            let match_by_path = pattern.contains('/');
            let mut results: Vec<String> = Vec::new();
            let mut filtered_sensitive = 0usize;
            for file in files {
                // 含 / 的模式匹配相对 root 的完整路径；不含 / 只比文件名
                let relative = file
                    .strip_prefix(&root)
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|_| file.clone());
                let matched = if match_by_path {
                    glob_pattern.matches_path(&relative)
                } else {
                    file.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|name| glob_pattern.matches(name))
                };
                if !matched {
                    continue;
                }
                if is_sensitive_file(&file) {
                    filtered_sensitive += 1;
                    continue;
                }
                // 输出相对 cwd（path 参数指向子目录时保留前缀）
                let display = file
                    .strip_prefix(ctx.cwd)
                    .map(|p| p.to_path_buf())
                    .unwrap_or(file);
                results.push(display.to_string_lossy().replace('\\', "/"));
                if results.len() >= MAX_MATCH_RESULTS {
                    break;
                }
            }
            let mut parts: Vec<String> = Vec::new();
            if !results.is_empty() {
                parts.push(results.join("\n"));
            }
            if results.len() >= MAX_MATCH_RESULTS {
                parts.push(format!("[结果过多，已截断为前 {MAX_MATCH_RESULTS} 条]"));
            }
            if filtered_sensitive > 0 {
                parts.push(format!("[已过滤 {filtered_sensitive} 个敏感文件]"));
            }
            let out = if parts.is_empty() {
                "（无匹配文件）".to_string()
            } else {
                parts.join("\n\n")
            };
            Ok(ToolEffect::plain(out))
        })
    }
}

impl Tool for Grep {
    fn name(&self) -> &'static str {
        "Grep"
    }

    fn read_only(&self) -> bool {
        true
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "Grep",
                "description": "用正则搜索工作区文件内容，输出 文件:行号: 内容。尊重 .gitignore/.ignore，包含隐藏文件，跳过敏感文件（.env/私钥等），文件按最近修改优先搜索。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "description": "正则表达式" },
                        "path": { "type": "string", "description": "搜索目录或单文件（相对工作目录），默认工作目录" },
                        "include": { "type": "string", "description": "文件名过滤 glob（如 *.rs）" },
                        "ignore_case": { "type": "boolean", "description": "true 时忽略大小写（默认 false）" }
                    },
                    "required": ["pattern"]
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
            let pattern = args["pattern"].as_str().ok_or("缺少参数 pattern")?;
            let ignore_case = args["ignore_case"].as_bool().unwrap_or(false);
            let regex = regex::RegexBuilder::new(pattern)
                .case_insensitive(ignore_case)
                .build()
                .map_err(|e| format!("无效正则 {pattern}: {e}"))?;
            let include = args["include"].as_str().map(|s| s.to_string());
            let root = match args["path"].as_str() {
                Some(path) => resolve_with_access(ctx.state, ctx.cwd, path, false, FsAccess::Read)?,
                None => ctx.cwd.to_path_buf(),
            };

            // 单文件直接搜；目录走工作区遍历并按 mtime 降序（截断时保留最近改动的文件）
            let mut files = Vec::new();
            if root.is_file() {
                files.push(root);
            } else {
                files = walk_workspace(&root);
                sort_by_mtime_desc(&mut files);
            }
            let mut out: Vec<String> = Vec::new();
            let mut skipped_sensitive = 0usize;
            'files: for file in files {
                if is_sensitive_file(&file) {
                    skipped_sensitive += 1;
                    continue;
                }
                if let Some(include) = &include {
                    let name = file.file_name().unwrap_or_default().to_string_lossy();
                    let glob_pattern = glob::Pattern::new(include)
                        .map_err(|e| format!("无效 include 模式 {include}: {e}"))?;
                    if !glob_pattern.matches(&name) {
                        continue;
                    }
                }
                if file.metadata().map(|m| m.len()).unwrap_or(0) > MAX_GREP_FILE_SIZE {
                    continue;
                }
                let Ok(content) = std::fs::read_to_string(&file) else {
                    continue;
                };
                if content.contains('\0') {
                    continue;
                }
                let relative = file
                    .strip_prefix(ctx.cwd)
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|_| file.clone());
                let relative = relative.to_string_lossy().replace('\\', "/");
                for (line_no, line) in content.lines().enumerate() {
                    if regex.is_match(line) {
                        out.push(format!("{relative}:{}: {line}", line_no + 1));
                        if out.len() >= MAX_MATCH_RESULTS {
                            out.push(format!("[结果过多，已截断为前 {MAX_MATCH_RESULTS} 行]"));
                            break 'files;
                        }
                    }
                }
            }
            if skipped_sensitive > 0 {
                out.push(format!("[已跳过 {skipped_sensitive} 个敏感文件]"));
            }
            if out.is_empty() {
                return Ok(ToolEffect::plain("（无匹配内容）".to_string()));
            }
            Ok(ToolEffect::plain(out.join("\n")))
        })
    }
}

/// 工作区遍历（ripgrep 同款 ignore 引擎）：尊重 .gitignore/.ignore/.git exclude，
/// 包含隐藏文件，但始终跳过 VCS 目录（.git/.svn/.hg/.bzr/.jj/.sl）与 .pigcode。
/// 只收集文件路径。
fn walk_workspace(root: &Path) -> Vec<PathBuf> {
    const SKIP_DIRS: &[&str] = &[".git", ".svn", ".hg", ".bzr", ".jj", ".sl", ".pigcode"];
    ignore::WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .ignore(true)
        .parents(true)
        .require_git(false)
        .filter_entry(|entry| {
            !(entry.file_type().is_some_and(|t| t.is_dir())
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| SKIP_DIRS.contains(&name)))
        })
        .build()
        .flatten()
        .filter(|entry| entry.file_type().is_some_and(|t| t.is_file()))
        .map(|entry| entry.into_path())
        .collect()
}

/// mtime（纳秒，UNIX 纪元起）；取不到当 0
fn mtime_nanos(path: &Path) -> u128 {
    path.metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// ZCode/kimi 同款排序：mtime 降序，同 mtime 按路径字典序升序
fn sort_by_mtime_desc(files: &mut Vec<PathBuf>) {
    let mut stamped: Vec<(u128, PathBuf)> = std::mem::take(files)
        .into_iter()
        .map(|path| (mtime_nanos(&path), path))
        .collect();
    stamped.sort_by(|(ma, pa), (mb, pb)| mb.cmp(ma).then_with(|| pa.cmp(pb)));
    files.extend(stamped.into_iter().map(|(_, path)| path));
}

/// 保守的破坏性命令黑名单（非 AST，只拦明确形态；命中即拒绝并说明理由）。
/// 宁可误拦也不放行，误拦文案引导用户手动执行。返回 Some(原因) 表示应拦截。
pub fn is_dangerous_command(command: &str) -> Option<&'static str> {
    let tokens: Vec<&str> = command.split_whitespace().collect();
    let compact: String = command.chars().filter(|c| !c.is_whitespace()).collect();

    // fork 炸弹：:(){ :|:& };:
    if compact.contains(":(){") && compact.contains("|:&") {
        return Some("fork 炸弹");
    }

    // 命令位判定：首 token，或跟在 ; & | && || sudo then do if ! ( 之后
    let in_command_position = |i: usize| {
        if i == 0 {
            return true;
        }
        let prev = tokens[i - 1];
        prev.ends_with(';')
            || prev.ends_with('&')
            || matches!(
                prev,
                "|" | "&&" | "||" | ";" | "(" | "sudo" | "then" | "do" | "if" | "!"
            )
    };
    fn base_name(token: &str) -> &str {
        token.rsplit('/').next().unwrap_or(token)
    }

    for (i, token) in tokens.iter().enumerate() {
        let base = base_name(token);
        if !in_command_position(i) {
            continue;
        }
        // rm -rf/-fr 且目标为 / /* ~ ~/ $HOME .（普通 rm -rf node_modules 放行）
        if base == "rm" {
            let mut recursive_force = false;
            let mut dangerous_target = false;
            for t in &tokens[i + 1..] {
                if t.starts_with('-') && !t.starts_with("--") {
                    let flags = t.trim_start_matches('-');
                    if (flags.contains('r') || flags.contains('R')) && flags.contains('f') {
                        recursive_force = true;
                    }
                } else if matches!(*t, "/" | "/*" | "~" | "~/" | "$HOME" | ".") {
                    dangerous_target = true;
                }
            }
            if recursive_force && dangerous_target {
                return Some("rm -rf 指向根/家/当前目录");
            }
        }
        // 磁盘格式化/分区
        if base == "mkfs" || base.starts_with("mkfs.") || base == "fdisk" {
            return Some("磁盘格式化/分区操作");
        }
        if base == "diskutil"
            && tokens
                .get(i + 1)
                .is_some_and(|next| next.starts_with("erase"))
        {
            return Some("磁盘格式化/分区操作");
        }
        // dd 写块设备（of=/dev/…，字符设备白名单放行）
        if base == "dd" {
            for t in &tokens[i + 1..] {
                if let Some(target) = t.strip_prefix("of=") {
                    if target.starts_with("/dev/")
                        && !matches!(
                            target,
                            "/dev/null" | "/dev/zero" | "/dev/random" | "/dev/urandom"
                        )
                    {
                        return Some("dd 写入块设备");
                    }
                }
            }
        }
        // 关机/重启
        if matches!(base, "shutdown" | "reboot" | "halt" | "poweroff") {
            return Some("关机/重启操作");
        }
        if base == "systemctl"
            && tokens
                .get(i + 1)
                .is_some_and(|next| matches!(*next, "poweroff" | "reboot" | "halt" | "kexec"))
        {
            return Some("关机/重启操作");
        }
        if base == "init"
            && tokens
                .get(i + 1)
                .is_some_and(|next| matches!(*next, "0" | "6"))
        {
            return Some("关机/重启操作");
        }
        // 递归改权/改属根目录
        if base == "chmod" || base == "chown" {
            let recursive = tokens[i + 1..]
                .iter()
                .any(|t| t.starts_with('-') && t.trim_start_matches('-').contains('R'));
            let root_target = tokens[i + 1..].iter().any(|t| *t == "/");
            let is_777 = tokens[i + 1..].iter().any(|t| *t == "777");
            if recursive && root_target && (base == "chown" || is_777) {
                return Some("递归改权/改属根目录");
            }
        }
        // git push --force / -f 不拦（常见操作，审批模式兜底）
    }
    None
}

impl Tool for Bash {
    fn name(&self) -> &'static str {
        "Bash"
    }

    fn is_shell(&self) -> bool {
        true
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "Bash",
                "description": "执行 shell 命令并返回 stdout/stderr 与退出码。工作目录为工作区根。高风险命令会弹窗请用户确认。注入 NO_COLOR=1 / TERM=dumb / GIT_TERMINAL_PROMPT=0（git 不会交互提问挂死）。timeout 默认 60s 最大 300s，超时自动转后台任务继续跑（输出不丢）；输出超 30KB 时完整内容落盘 .pigcode/tool-results/ 并返回头尾预览。长时命令（dev server/watch/长构建）也可用 run_in_background 直接后台运行。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "要执行的命令" },
                        "run_in_background": { "type": "boolean", "description": "true 时后台运行，立即返回 task_id（默认 false）" },
                        "timeout": { "type": "integer", "description": "超时秒数，默认 60，最大 300；超时后命令自动转入后台继续运行，不丢输出" }
                    },
                    "required": ["command"]
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
            let command = args["command"].as_str().ok_or("缺少参数 command")?;
            // 黑名单（is_dangerous_command）的拦截在会话层：命中 → 强制审批弹窗，
            // 用户 Allow 才走到这里；execute 层不再硬拒。
            if args["run_in_background"].as_bool().unwrap_or(false) {
                let task_id = crate::task::spawn_background(ctx.state, ctx.cwd, command);
                return Ok(ToolEffect::plain(format!(
                    "已在后台启动，task_id: {task_id}。用 TaskOutput 查看输出，TaskStop 停止。"
                )));
            }
            let secs = args["timeout"].as_u64().unwrap_or(60).clamp(1, 300);
            match crate::task::run_foreground(
                ctx.state,
                ctx.cwd,
                command,
                std::time::Duration::from_secs(secs),
            )
            .await
            {
                crate::task::ForegroundOutcome::SpawnFailed { error } => {
                    Err(format!("启动命令失败: {error}"))
                }
                crate::task::ForegroundOutcome::TimedOut { task_id } => {
                    Ok(ToolEffect::plain(format!(
                        "命令超过 {secs}s 未结束，已转入后台任务 {task_id}（输出持续保留）。用 TaskOutput 查看，TaskStop 停止。"
                    )))
                }
                crate::task::ForegroundOutcome::Completed {
                    output,
                    code,
                    spill_path,
                } => {
                    let mut text = output;
                    if text.chars().count() <= MAX_BASH_OUTPUT {
                        // 小输出不留痕：清掉 spill 文件
                        if let Some(path) = &spill_path {
                            let _ = std::fs::remove_file(path);
                        }
                        text.push_str(&format!("\n[exit code: {code}]"));
                    } else {
                        // 头尾预览 + 全量在 spill 文件（注册表 output 有 64KB 滚动上限，不作数）
                        let total = text.chars().count();
                        let head: String = text.chars().take(4096).collect();
                        let tail = crate::task::tail_chars(&text, 1024);
                        let spill_display = spill_path
                            .as_ref()
                            .map(|path| {
                                path.strip_prefix(ctx.cwd)
                                    .map(|relative| relative.to_string_lossy().replace('\\', "/"))
                                    .unwrap_or_else(|_| path.display().to_string())
                            })
                            .unwrap_or_else(|| "（未知路径）".to_string());
                        text = format!(
                            "{head}\n\n[...中间省略...]\n\n{tail}\n\n[输出过长（共 {total} 字符），完整输出已保存到 {spill_display}，可用 Read 分页查看]\n[exit code: {code}]"
                        );
                    }
                    Ok(ToolEffect::plain(text))
                }
            }
        })
    }
}

struct TodoListTool;

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

const MAX_FETCH_BODY: usize = 2 * 1024 * 1024;
const MAX_FETCH_OUTPUT: usize = 50000;

/// 数值 IP 判定：未指定/环回/私网/链路本地/文档段/CGNAT/benchmark/组播。
/// FetchURL 的字面 host 与 DNS 解析结果共用。
pub fn is_private_ip(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            let b = v4.octets();
            v4.is_unspecified()
                || v4.is_loopback()
                || v4.is_private() // 10/8、172.16/12、192.168/16
                || v4.is_link_local() // 169.254/16
                || v4.is_documentation()
                || (b[0] == 100 && (64..=127).contains(&b[1])) // 100.64.0.0/10 CGNAT
                || (b[0] == 198 && (b[1] == 18 || b[1] == 19)) // 198.18.0.0/15 benchmark
                || v4.is_multicast()
        }
        std::net::IpAddr::V6(v6) => {
            let b = v6.octets();
            v6.is_unspecified()
                || v6.is_loopback()
                || (b[0] & 0xfe) == 0xfc // fc00::/7 unique local
                || (b[0] == 0xfe && (b[1] & 0xc0) == 0x80) // fe80::/10 link-local
                || v6.is_multicast()
        }
    }
}

/// SSRF 防护：IP 字面量走数值判定（见 is_private_ip）；
/// 域名拒绝 localhost 家族（含 *.localhost）与单段主机名（内网短名）。
/// DNS 解析出的地址由 is_private_ip 逐跳校验。
pub fn is_private_host(host: &str) -> bool {
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return is_private_ip(&ip);
    }
    host == "localhost" || host.ends_with(".localhost") || !host.contains('.')
}

/// 从 HTML 提取正文：剔除 script/style/noscript/svg/template，优先 main/article
/// 否则 body；块级元素之间换行，行内空白压缩，连续空行折叠。
pub fn extract_text(html: &str) -> String {
    use scraper::{Html, Selector};
    const SKIP: &[&str] = &["script", "style", "noscript", "svg", "template"];
    const BLOCK: &[&str] = &[
        "address",
        "article",
        "aside",
        "blockquote",
        "br",
        "dd",
        "details",
        "div",
        "dl",
        "dt",
        "fieldset",
        "figcaption",
        "figure",
        "footer",
        "form",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "header",
        "hr",
        "li",
        "main",
        "nav",
        "ol",
        "p",
        "pre",
        "section",
        "table",
        "td",
        "th",
        "tr",
        "ul",
    ];
    let document = Html::parse_document(html);
    let mut root = None;
    for name in ["main", "article", "body"] {
        let selector = Selector::parse(name).expect("合法选择器");
        if let Some(el) = document.select(&selector).next() {
            root = Some(el);
            break;
        }
    }
    let Some(root) = root else {
        return String::new();
    };
    // 栈遍历（None = 块级元素闭合，补换行）；(*root) 解引用到 NodeRef 以遍历文本节点
    let mut out = String::new();
    let mut stack: Vec<Option<_>> = (*root)
        .children()
        .map(Some)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    while let Some(item) = stack.pop() {
        let Some(node) = item else {
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            continue;
        };
        match node.value() {
            scraper::Node::Text(text) => {
                // 标签之间的纯空白是排版噪音，丢弃（行内空白后续统一压缩）
                if !text.text.trim().is_empty() {
                    out.push_str(&text.text);
                }
            }
            scraper::Node::Element(el) => {
                let name = el.name();
                if SKIP.contains(&name) {
                    continue;
                }
                let block = BLOCK.contains(&name);
                if block && !out.is_empty() && !out.ends_with('\n') {
                    out.push('\n');
                }
                if block {
                    stack.push(None);
                }
                stack.extend(
                    node.children()
                        .map(Some)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev(),
                );
            }
            _ => {}
        }
    }
    let mut lines: Vec<String> = Vec::new();
    for line in out.lines() {
        let collapsed = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if collapsed.is_empty() && lines.last().is_none_or(|l| l.is_empty()) {
            continue;
        }
        lines.push(collapsed);
    }
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

struct FetchUrl;

impl Tool for FetchUrl {
    fn name(&self) -> &'static str {
        "FetchURL"
    }

    fn read_only(&self) -> bool {
        true
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "FetchURL",
                "description": "抓取公开网页并提取正文（HTML 自动清洗为纯文本，JSON/纯文本原样返回）。不支持需要登录的页面。URL 不允许内嵌凭据，域名会先做 DNS 私网校验。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "url": { "type": "string", "description": "要抓取的 http/https URL" }
                    },
                    "required": ["url"]
                }
            }
        })
    }

    fn execute<'a>(
        &'a self,
        args: serde_json::Value,
        _ctx: ToolContext<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<ToolEffect, String>> + Send + 'a>> {
        Box::pin(async move {
            let url = args["url"].as_str().ok_or("缺少参数 url")?;
            let mut current = reqwest::Url::parse(url).map_err(|e| format!("URL 无效: {e}"))?;
            // 手动跟随重定向：每跳都完整重做 凭据/字面 IP/DNS 校验并钉死解析结果
            let mut hops = 0;
            let mut response = loop {
                let client = pinned_client(&current).await?;
                let response = client
                    .get(current.clone())
                    .send()
                    .await
                    .map_err(|e| format!("请求失败: {e}"))?;
                if !response.status().is_redirection() {
                    break response;
                }
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok());
                let Some(location) = location else {
                    break response; // 3xx 无 Location：当终态（HTTP 状态检查会拦下）
                };
                hops += 1;
                if hops > 5 {
                    return Err("重定向次数过多".to_string());
                }
                current = current
                    .join(location)
                    .map_err(|e| format!("重定向 URL 无效: {e}"))?;
            };
            let status = response.status();
            if !status.is_success() {
                return Err(format!("HTTP {status}"));
            }
            let content_type = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(|s| {
                    s.split(';')
                        .next()
                        .unwrap_or("")
                        .trim()
                        .to_ascii_lowercase()
                })
                .unwrap_or_default();
            // 流式读体，上限 2MB
            let mut body: Vec<u8> = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|e| format!("读取响应失败: {e}"))?
            {
                let remaining = MAX_FETCH_BODY.saturating_sub(body.len());
                if chunk.len() > remaining {
                    body.extend_from_slice(&chunk[..remaining]);
                    break;
                }
                body.extend_from_slice(&chunk);
            }
            let text = String::from_utf8_lossy(&body).to_string();
            let mut out = match content_type.as_str() {
                "text/html" => extract_text(&text),
                "text/plain" | "text/markdown" | "application/json" => text,
                "" => return Err("响应缺少 Content-Type，无法判定内容类型".to_string()),
                other => return Err(format!("不支持的内容类型: {other}")),
            };
            if out.is_empty() {
                out = "（页面无可提取文本）".to_string();
            }
            if out.chars().count() > MAX_FETCH_OUTPUT {
                out = out.chars().take(MAX_FETCH_OUTPUT).collect();
                out.push_str("\n\n（已截断，仅显示前 50000 字符）");
            }
            Ok(ToolEffect::plain(out))
        })
    }
}

/// FetchURL 的 URL 静态校验：scheme 白名单 + 内嵌凭据拒绝 + 字面 host 私网判定
///（域名的 DNS 校验在 pinned_client 里做）。
pub fn check_fetch_url(url: &reqwest::Url) -> Result<(), String> {
    match url.scheme() {
        "http" | "https" => {}
        scheme => return Err(format!("仅支持 http/https URL（收到 {scheme}:）")),
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("URL 不允许内嵌凭据".to_string());
    }
    let host = url.host_str().ok_or("URL 缺少主机名")?;
    if is_private_host(host) {
        return Err(format!("不允许访问本机/私网地址: {host}"));
    }
    Ok(())
}

/// 每跳新建 pinned client：静态校验后，域名先解析（spawn_blocking + ToSocketAddrs），
/// 所有结果逐个过 is_private_ip，任一私网即拒绝；全公网则 resolve_to_addrs 钉死，
/// 防 check-to-connect 之间的 DNS rebinding。
/// 注意：使用系统代理时代理自行解析 DNS，钉生不对代理生效，属已知取舍。
async fn pinned_client(url: &reqwest::Url) -> Result<reqwest::Client, String> {
    use std::net::ToSocketAddrs as _;
    check_fetch_url(url)?;
    let host = url.host_str().expect("check_fetch_url 已校验");
    let mut builder = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent("pig-code FetchURL/0.1 (coding agent)")
        .redirect(reqwest::redirect::Policy::none()); // 重定向手动跟随，每跳重验
    let is_ip_literal = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<std::net::IpAddr>()
        .is_ok();
    if !is_ip_literal {
        let port = url
            .port_or_known_default()
            .unwrap_or(if url.scheme() == "https" { 443 } else { 80 });
        let host_owned = host.to_string();
        let resolved = tokio::task::spawn_blocking(move || {
            (host_owned.as_str(), port)
                .to_socket_addrs()
                .map(|addrs| addrs.collect::<Vec<_>>())
        })
        .await
        .map_err(|e| format!("DNS 解析失败: {host}: {e}"))?
        .map_err(|_| format!("DNS 解析失败: {host}"))?;
        if resolved.is_empty() {
            return Err(format!("DNS 解析失败: {host}"));
        }
        if resolved.iter().any(|addr| is_private_ip(&addr.ip())) {
            return Err(format!("域名解析到私网/保留地址，已拒绝: {host}"));
        }
        builder = builder.resolve_to_addrs(host, &resolved);
    }
    builder.build().map_err(|e| e.to_string())
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

struct TaskList;

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

struct TaskOutput;

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

struct TaskStop;

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

struct AskUserQuestionTool;

/// ExitPlanMode：模型请求退出计划模式。会话层在 Plan 硬拒之前拦截并强制弹窗
/// （ZCode 同款）；工具实现只是防御性兜底，正常路径不会走到 execute。
struct ExitPlanModeTool;
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
struct EnterPlanModeTool;

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

/// @ 文件搜索：遍历工作区（尊重 .gitignore、含隐藏文件、跳过 VCS 目录），按子串匹配打分排序。
pub fn search_files(cwd: &Path, query: &str, limit: usize) -> Vec<String> {
    let files = walk_workspace(cwd);
    let query = query.to_lowercase();
    let mut scored: Vec<(u64, String)> = files
        .iter()
        .filter_map(|file| {
            let relative = file
                .strip_prefix(cwd)
                .ok()?
                .to_string_lossy()
                .replace('\\', "/");
            if query.is_empty() {
                return Some((relative.len() as u64 + 1000, relative));
            }
            let lower = relative.to_lowercase();
            lower
                .find(&query)
                .map(|pos| (pos as u64 * 1000 + relative.len() as u64, relative))
        })
        .collect();
    scored.sort();
    scored.truncate(limit);
    scored.into_iter().map(|(_, path)| path).collect()
}
