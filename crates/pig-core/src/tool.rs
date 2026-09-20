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
}

impl ToolEffect {
    fn plain(output: String) -> Self {
        Self {
            output,
            file_change: None,
        }
    }
}

/// 会话级变更追踪：首次修改前快照原始内容，diff 始终是「原始 → 当前」。
#[derive(Default)]
pub struct ChangeTracker {
    originals: HashMap<PathBuf, Option<String>>,
    stats: HashMap<PathBuf, (u32, u32)>,
}

impl ChangeTracker {
    /// 修改前快照；返回原始内容（None = 文件原本不存在）。
    pub fn snapshot(&mut self, path: &Path) -> Result<Option<String>, String> {
        if let std::collections::hash_map::Entry::Vacant(entry) = self.originals.entry(path.to_path_buf()) {
            let original = match std::fs::read_to_string(path) {
                Ok(content) => Some(content),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(format!("读取失败 {}: {e}", path.display())),
            };
            entry.insert(original);
        }
        Ok(self.originals[path].clone())
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
        self.stats.insert(path.to_path_buf(), (additions, deletions));
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

pub struct ToolContext<'a> {
    pub cwd: &'a Path,
    pub tracker: &'a mut ChangeTracker,
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
        "read_file" | "write_file" | "edit" => args["path"].as_str().unwrap_or("?").to_string(),
        "bash" => args["command"].as_str().unwrap_or("?").to_string(),
        "glob" => args["pattern"].as_str().unwrap_or("?").to_string(),
        "grep" => args["pattern"].as_str().unwrap_or("?").to_string(),
        _ => args.to_string(),
    };
    if raw.chars().count() <= 80 {
        raw
    } else {
        format!("{}…", raw.chars().take(80).collect::<String>())
    }
}

pub async fn execute(call: &ToolCall, ctx: ToolContext<'_>) -> (String, bool, Option<FileChange>) {
    let args: serde_json::Value = serde_json::from_str(&call.arguments).unwrap_or_default();
    let tools = all();
    let Some(tool) = tools.iter().find(|tool| tool.name() == call.name) else {
        return (format!("未知工具: {}", call.name), true, None);
    };
    match tool.execute(args, ctx).await {
        Ok(effect) => (effect.output, false, effect.file_change),
        Err(error) => (error, true, None),
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
        "read_file"
    }

    fn read_only(&self) -> bool {
        true
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "read_file",
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
        "write_file"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "write_file",
                "description": "写入整个文件（自动创建父目录）。大文件优先用 edit 做局部修改。",
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
            ctx.tracker.snapshot(&full)?;
            std::fs::write(&full, content)
                .map_err(|e| format!("写入失败 {}: {e}", full.display()))?;
            let file_change = ctx.tracker.diff(ctx.cwd, &full).ok();
            Ok(ToolEffect {
                output: format!("已写入 {}（{} 字节）", path, content.len()),
                file_change,
            })
        })
    }
}

impl Tool for EditFile {
    fn name(&self) -> &'static str {
        "edit"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "edit",
                "description": "精确替换文件中的文本。old_string 必须在文件中唯一出现；先 read_file 确认内容再改。",
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
                    "old_string 在 {path} 中未找到。请先用 read_file 确认文件当前内容（注意缩进与换行需完全一致）。"
                ));
            }
            if count > 1 {
                return Err(format!(
                    "old_string 在 {path} 中出现 {count} 次，无法唯一定位。请扩大 old_string 范围使其唯一。"
                ));
            }
            ctx.tracker.snapshot(&full)?;
            std::fs::write(&full, content.replacen(old, new, 1))
                .map_err(|e| format!("写入失败 {}: {e}", full.display()))?;
            let file_change = ctx.tracker.diff(ctx.cwd, &full).ok();
            Ok(ToolEffect {
                output: format!("已修改 {path}"),
                file_change,
            })
        })
    }
}

impl Tool for Glob {
    fn name(&self) -> &'static str {
        "glob"
    }

    fn read_only(&self) -> bool {
        true
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "glob",
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
            let entries = glob::glob(&pattern_str)
                .map_err(|e| format!("无效 glob 模式 {pattern}: {e}"))?;
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
                out.push_str(&format!("\n\n[结果过多，已截断为前 {MAX_MATCH_RESULTS} 条]"));
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
        "grep"
    }

    fn read_only(&self) -> bool {
        true
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "grep",
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
            let regex = regex::Regex::new(pattern)
                .map_err(|e| format!("无效正则 {pattern}: {e}"))?;
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
        "bash"
    }

    fn is_shell(&self) -> bool {
        true
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "bash",
                "description": "执行 shell 命令并返回 stdout/stderr 与退出码。工作目录为项目根。禁止破坏性命令。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "要执行的命令" }
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
            text.push_str(&format!("\n[exit code: {}]", output.status.code().unwrap_or(-1)));
            Ok(ToolEffect::plain(text))
        })
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
