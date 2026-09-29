use super::*;

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
pub(crate) fn decoded_view(bytes: &[u8]) -> String {
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

/// 持久化 String → 原始字节：有 hex 前缀则解码；无前缀 = 写入端直存的合法 UTF-8
///（snapshot_to_store 只对非 UTF-8 字节加 hex 前缀，UTF-8 原文不落前缀）
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
