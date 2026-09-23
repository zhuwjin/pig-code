use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use pig_protocol::ExecMode;

use crate::provider::ToolCall;

const MAX_READ_LINES: usize = 2000;
const MAX_MATCH_RESULTS: usize = 200;
const MAX_BASH_OUTPUT: usize = 30 * 1024;
const BASH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
const MAX_GREP_FILE_SIZE: u64 = 2 * 1024 * 1024;

pub struct FileChange {
    pub path: String,
    pub unified_diff: String,
    pub additions: u32,
    pub deletions: u32,
}

pub struct ToolEffect {
    pub output: String,
    pub file_change: Option<FileChange>,
    /// 本次编辑自身的 diff（「编辑前 → 编辑后」），UI 工具卡片内联渲染用；
    /// `file_change` 是会话累计口径（review 面板用），两者粒度不同
    pub edit_diff: Option<FileChange>,
}

impl ToolEffect {
    fn plain(output: String) -> Self {
        Self {
            output,
            file_change: None,
            edit_diff: None,
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

/// 会话级变更追踪：首次修改前快照原始内容，diff 始终是「原始 → 当前」。
/// 快照经 dirty 标记由 session 侧落盘（file_originals 表），重启后 restore 恢复基线。
///
/// 另有一层**每轮**追踪（ZCode turn-file-changes 同款口径）：turn 内首次写前
/// 记录 turn_originals，回合结束 take_turn_changes 算「本轮首次写前 → 当前」净额并清空。
#[derive(Default)]
pub struct ChangeTracker {
    originals: HashMap<PathBuf, Option<String>>,
    stats: HashMap<PathBuf, (u32, u32)>,
    /// 本次进程内新增、尚未落盘的快照路径（session 侧 drain 后写库）
    dirty: Vec<PathBuf>,
    /// 本轮内各文件首次写前的内容（None = 本轮新建）；回合结束 take 清空
    turn_originals: HashMap<PathBuf, Option<String>>,
}

impl ChangeTracker {
    /// 修改前快照；返回原始内容（None = 文件原本不存在）。
    pub fn snapshot(&mut self, path: &Path) -> Result<Option<String>, String> {
        if let std::collections::hash_map::Entry::Vacant(entry) =
            self.originals.entry(path.to_path_buf())
        {
            let original = match std::fs::read_to_string(path) {
                Ok(content) => Some(content),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(format!("读取失败 {}: {e}", path.display())),
            };
            entry.insert(original);
            self.dirty.push(path.to_path_buf());
        }
        // 每轮口径：本轮首次写前同样记一笔（独立于会话级基线）
        if let std::collections::hash_map::Entry::Vacant(entry) =
            self.turn_originals.entry(path.to_path_buf())
        {
            let original = match std::fs::read_to_string(path) {
                Ok(content) => Some(content),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(format!("读取失败 {}: {e}", path.display())),
            };
            entry.insert(original);
        }
        Ok(self.originals[path].clone())
    }

    /// 取出新增快照路径（落盘后清空）
    pub fn take_dirty(&mut self) -> Vec<PathBuf> {
        std::mem::take(&mut self.dirty)
    }

    /// 读取某路径的原始快照（None 值 = 文件原本不存在；None 返回 = 未追踪）
    pub fn original(&self, path: &Path) -> Option<Option<String>> {
        self.originals.get(path).cloned()
    }

    /// 重启后恢复基线（来自 file_originals 表；恢复的不标 dirty，避免回写）
    pub fn restore(&mut self, entries: Vec<(PathBuf, Option<String>)>) {
        for (path, original) in entries {
            self.originals.insert(path, original);
        }
    }

    /// 生成「原始 → 当前」的 unified diff 与增删行数。
    pub fn diff(&mut self, cwd: &Path, path: &Path) -> Result<FileChange, String> {
        let original = self
            .originals
            .get(path)
            .ok_or_else(|| "文件未追踪".to_string())?
            .clone()
            .unwrap_or_default();
        let current = std::fs::read_to_string(path)
            .map_err(|e| format!("读取失败 {}: {e}", path.display()))?;
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

    /// 计算并清空「本轮改动」：每文件 本轮首次写前 → 当前磁盘 的净 diff。
    /// 净额为零（本轮内改回原文）不产出；文件被删除的暂不产出。
    pub fn take_turn_changes(&mut self, cwd: &Path) -> Vec<FileChange> {
        let entries = std::mem::take(&mut self.turn_originals);
        let mut changes = Vec::new();
        for (path, before) in entries {
            let before = before.unwrap_or_default();
            let Ok(current) = std::fs::read_to_string(&path) else {
                continue;
            };
            let change = per_edit_diff(cwd, &path, &before, &current);
            if change.additions == 0 && change.deletions == 0 {
                continue;
            }
            changes.push(change);
        }
        changes.sort_by(|a, b| a.path.cmp(&b.path));
        changes
    }

    pub fn revert(&mut self, path: &Path) -> Result<(), String> {
        let Some(original) = self.originals.remove(path) else {
            return Err("文件未被修改过，无法撤销".to_string());
        };
        self.stats.remove(path);
        match original {
            Some(content) => std::fs::write(path, content)
                .map_err(|e| format!("恢复失败 {}: {e}", path.display())),
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
    ]
}

pub fn schemas() -> Vec<serde_json::Value> {
    all().iter().map(|tool| tool.schema()).collect()
}

/// 审批判定：Plan 模式在更早处拦截（直接拒绝），这里只管其余三档。
pub fn requires_approval(tool: &dyn Tool, mode: ExecMode) -> bool {
    match mode {
        ExecMode::FullAccess => false,
        ExecMode::Plan => false,
        ExecMode::ConfirmBeforeEdit => !tool.read_only(),
        ExecMode::AutoEdit => tool.is_shell(),
    }
}

pub fn summarize(call: &ToolCall) -> String {
    let args: serde_json::Value = serde_json::from_str(&call.arguments).unwrap_or_default();
    let raw = match call.name.as_str() {
        "Read" | "Write" | "Edit" => args["path"].as_str().unwrap_or("?").to_string(),
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
) {
    let args: serde_json::Value = serde_json::from_str(&call.arguments).unwrap_or_default();
    let tools = all();
    let Some(tool) = tools.iter().find(|tool| tool.name() == call.name) else {
        return (format!("未知工具: {}", call.name), true, None, None);
    };
    match tool.execute(args, ctx).await {
        Ok(effect) => (
            effect.output,
            false,
            effect.file_change,
            effect.edit_diff.map(Into::into),
        ),
        Err(error) => (error, true, None, None),
    }
}

/// 解析相对 cwd 的路径并防止越出工作目录。
pub fn resolve_checked(cwd: &Path, path: &str, create_parents: bool) -> Result<PathBuf, String> {
    let raw = Path::new(path);
    let full = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        cwd.join(raw)
    };
    let cwd_canonical = cwd
        .canonicalize()
        .map_err(|e| format!("工作目录无效 {}: {e}", cwd.display()))?;
    let parent = full.parent().unwrap_or(cwd);
    if create_parents {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("创建目录失败 {}: {e}", parent.display()))?;
    }
    let parent_canonical = parent
        .canonicalize()
        .map_err(|e| format!("目录不存在 {}: {e}", parent.display()))?;
    if !parent_canonical.starts_with(&cwd_canonical) {
        return Err(format!("路径越出工作目录: {path}"));
    }
    let file_name = full
        .file_name()
        .ok_or_else(|| format!("无效路径: {path}"))?;
    Ok(parent_canonical.join(file_name))
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
                "description": "读取工作区内文件内容。path 相对工作目录；文件过长时用 offset/limit 分页。",
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
            let full = resolve_checked(ctx.cwd, path, false)?;
            let content = std::fs::read_to_string(&full)
                .map_err(|e| format!("读取失败 {}: {e}", full.display()))?;
            let offset = args["offset"].as_u64().unwrap_or(1).max(1) as usize;
            let limit = args["limit"].as_u64().unwrap_or(MAX_READ_LINES as u64) as usize;
            let lines: Vec<&str> = content.lines().collect();
            let total = lines.len();
            let start = (offset - 1).min(total);
            let end = (start + limit).min(total);
            let mut out = lines[start..end].join("\n");
            if end < total {
                out.push_str(&format!(
                    "\n\n[已截断: 显示 {}-{end} 行，共 {total} 行；用 offset 参数继续读取]",
                    start + 1
                ));
            }
            Ok(ToolEffect::plain(out))
        })
    }
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
            let full = resolve_checked(ctx.cwd, path, true)?;
            let before = std::fs::read_to_string(&full).unwrap_or_default();
            ctx.tracker.snapshot(&full)?;
            std::fs::write(&full, content)
                .map_err(|e| format!("写入失败 {}: {e}", full.display()))?;
            let file_change = ctx.tracker.diff(ctx.cwd, &full).ok();
            let edit_diff = Some(per_edit_diff(ctx.cwd, &full, &before, content));
            Ok(ToolEffect {
                output: format!("已写入 {}（{} 字节）", path, content.len()),
                file_change,
                edit_diff,
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
                "description": "精确替换文件中的文本。old_string 必须在文件中唯一出现；先 Read 确认内容再改。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "相对工作目录的文件路径" },
                        "old_string": { "type": "string", "description": "要被替换的原文（须唯一出现）" },
                        "new_string": { "type": "string", "description": "替换后的新文本" }
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
                return Err("old_string 不能为空".to_string());
            }
            let full = resolve_checked(ctx.cwd, path, false)?;
            let content = std::fs::read_to_string(&full)
                .map_err(|e| format!("读取失败 {}: {e}", full.display()))?;
            let count = content.matches(old).count();
            if count == 0 {
                return Err(format!(
                    "old_string 在 {path} 中未找到。请先用 Read 确认文件当前内容（注意缩进与换行需完全一致）。"
                ));
            }
            if count > 1 {
                return Err(format!(
                    "old_string 在 {path} 中出现 {count} 次，无法唯一定位。请扩大 old_string 范围使其唯一。"
                ));
            }
            ctx.tracker.snapshot(&full)?;
            let after = content.replacen(old, new, 1);
            std::fs::write(&full, &after)
                .map_err(|e| format!("写入失败 {}: {e}", full.display()))?;
            let file_change = ctx.tracker.diff(ctx.cwd, &full).ok();
            let edit_diff = Some(per_edit_diff(ctx.cwd, &full, &content, &after));
            Ok(ToolEffect {
                output: format!("已修改 {path}"),
                file_change,
                edit_diff,
            })
        })
    }
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
                "description": "按文件名模式匹配工作区文件（如 **/*.rs）。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "description": "glob 模式" },
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
            let root = match args["path"].as_str() {
                Some(path) => resolve_checked(ctx.cwd, path, false)?,
                None => ctx.cwd.to_path_buf(),
            };
            let full_pattern = root.join(pattern);
            let pattern_str = full_pattern.to_string_lossy().replace('\\', "/");
            let entries =
                glob::glob(&pattern_str).map_err(|e| format!("无效 glob 模式 {pattern}: {e}"))?;
            let mut results: Vec<String> = Vec::new();
            for entry in entries.flatten() {
                let relative = entry
                    .strip_prefix(ctx.cwd)
                    .map(|p| p.to_path_buf())
                    .unwrap_or(entry);
                results.push(relative.to_string_lossy().replace('\\', "/"));
                if results.len() >= MAX_MATCH_RESULTS {
                    break;
                }
            }
            results.sort();
            let mut out = results.join("\n");
            if results.len() >= MAX_MATCH_RESULTS {
                out.push_str(&format!(
                    "\n\n[结果过多，已截断为前 {MAX_MATCH_RESULTS} 条]"
                ));
            }
            if out.is_empty() {
                out = "（无匹配文件）".to_string();
            }
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
                "description": "用正则搜索工作区文件内容，输出 文件:行号: 内容。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "description": "正则表达式" },
                        "path": { "type": "string", "description": "搜索目录或单文件（相对工作目录），默认工作目录" },
                        "include": { "type": "string", "description": "文件名过滤 glob（如 *.rs）" }
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
            let regex =
                regex::Regex::new(pattern).map_err(|e| format!("无效正则 {pattern}: {e}"))?;
            let include = args["include"].as_str().map(|s| s.to_string());
            let root = match args["path"].as_str() {
                Some(path) => resolve_checked(ctx.cwd, path, false)?,
                None => ctx.cwd.to_path_buf(),
            };

            let mut files = Vec::new();
            if root.is_file() {
                files.push(root);
            } else {
                walk(&root, &mut files);
            }
            let mut out: Vec<String> = Vec::new();
            'files: for file in files {
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
            if out.is_empty() {
                return Ok(ToolEffect::plain("（无匹配内容）".to_string()));
            }
            Ok(ToolEffect::plain(out.join("\n")))
        })
    }
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    const SKIP: &[&str] = &[".git", "target", "node_modules", ".pigcode"];
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || SKIP.contains(&name.as_ref()) {
            continue;
        }
        if path.is_dir() {
            walk(&path, out);
        } else {
            out.push(path);
        }
    }
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
                "description": "执行 shell 命令并返回 stdout/stderr 与退出码。工作目录为工作区根。禁止破坏性命令。长时命令（dev server/watch/长构建）用 run_in_background 后台运行。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "要执行的命令" },
                        "run_in_background": { "type": "boolean", "description": "true 时后台运行，立即返回 task_id（默认 false）" }
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
            if args["run_in_background"].as_bool().unwrap_or(false) {
                let task_id = crate::task::spawn_background(ctx.state, ctx.cwd, command);
                return Ok(ToolEffect::plain(format!(
                    "已在后台启动，task_id: {task_id}。用 TaskOutput 查看输出，TaskStop 停止。"
                )));
            }
            let child = if cfg!(target_os = "windows") {
                tokio::process::Command::new("cmd")
                    .args(["/C", command])
                    .current_dir(ctx.cwd)
                    .output()
            } else {
                tokio::process::Command::new("sh")
                    .args(["-c", command])
                    .current_dir(ctx.cwd)
                    .output()
            };
            let output = tokio::time::timeout(BASH_TIMEOUT, child)
                .await
                .map_err(|_| format!("命令超时（{}s）", BASH_TIMEOUT.as_secs()))?
                .map_err(|e| format!("启动命令失败: {e}"))?;

            let mut text = String::new();
            text.push_str(&String::from_utf8_lossy(&output.stdout));
            if !output.stderr.is_empty() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str("[stderr]\n");
                text.push_str(&String::from_utf8_lossy(&output.stderr));
            }
            if text.len() > MAX_BASH_OUTPUT {
                text.truncate(MAX_BASH_OUTPUT);
                text.push_str("\n\n[输出过长，已截断]");
            }
            text.push_str(&format!(
                "\n[exit code: {}]",
                output.status.code().unwrap_or(-1)
            ));
            Ok(ToolEffect::plain(text))
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

/// SSRF 防护：拒绝本机/私网地址字面量（localhost、127/8、::1、0.0.0.0、
/// 10/8、192.168/16、172.16-31/12、169.254/16）。DNS 解析出的私网地址不在此列。
pub fn is_private_host(host: &str) -> bool {
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if host == "localhost" || host == "::1" || host == "0.0.0.0" {
        return true;
    }
    if host.starts_with("127.")
        || host.starts_with("10.")
        || host.starts_with("192.168.")
        || host.starts_with("169.254.")
    {
        return true;
    }
    if let Some(rest) = host.strip_prefix("172.") {
        if let Some(second) = rest.split('.').next().and_then(|s| s.parse::<u8>().ok()) {
            if (16..=31).contains(&second) {
                return true;
            }
        }
    }
    false
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
                "description": "抓取公开网页并提取正文（HTML 自动清洗为纯文本，JSON/纯文本原样返回）。不支持需要登录的页面。",
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
            let parsed = reqwest::Url::parse(url).map_err(|e| format!("URL 无效: {e}"))?;
            match parsed.scheme() {
                "http" | "https" => {}
                scheme => return Err(format!("仅支持 http/https URL（收到 {scheme}:）")),
            }
            let host = parsed.host_str().ok_or("URL 缺少主机名")?;
            if is_private_host(host) {
                return Err(format!("不允许访问本机/私网地址: {host}"));
            }
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .user_agent("pig-code FetchURL/0.1 (coding agent)")
                .redirect(reqwest::redirect::Policy::custom(|attempt| {
                    let private = attempt
                        .url()
                        .host_str()
                        .map(is_private_host)
                        .unwrap_or(false);
                    if private || attempt.previous().len() >= 5 {
                        attempt.stop()
                    } else {
                        attempt.follow()
                    }
                }))
                .build()
                .map_err(|e| e.to_string())?;
            let mut response = client
                .get(parsed)
                .send()
                .await
                .map_err(|e| format!("请求失败: {e}"))?;
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

/// @ 文件搜索：遍历工作区（跳过 .git/target/node_modules 等），按子串匹配打分排序。
pub fn search_files(cwd: &Path, query: &str, limit: usize) -> Vec<String> {
    let mut files = Vec::new();
    walk(cwd, &mut files);
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
