use std::path::Path;

use pig_protocol::GitFileChange;

/// untracked 文件行数统计上限（ZCode GIT_UNTRACKED_STAT_MAX_BYTES 同值）
const UNTRACKED_STAT_MAX_BYTES: u64 = 1024 * 1024;
/// 单文件 diff 原文上限（ZCode DEFAULT_GIT_DIFF_BYTES 同值）
const DIFF_MAX_BYTES: usize = 1024 * 1024;

fn run_git(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()
        .filter(|out| out.status.success())?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// 查询目录的 git 信息：当前分支 + 本地分支列表。非 git 仓库返回 (None, vec![])。
pub fn git_info(cwd: &Path) -> (Option<String>, Vec<String>) {
    let current = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(cwd)
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|name| !name.is_empty());

    let branches = std::process::Command::new("git")
        .args(["branch", "--format=%(refname:short)"])
        .current_dir(cwd)
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| {
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(|line| line.trim().to_string())
                .filter(|line| !line.is_empty())
                .collect()
        })
        .unwrap_or_default();

    (current, branches)
}

/// 切换分支；失败透传 git stderr。
pub fn checkout(cwd: &Path, branch: &str) -> Result<(), String> {
    let output = std::process::Command::new("git")
        .args(["checkout", branch])
        .current_dir(cwd)
        .output()
        .map_err(|e| format!("启动 git 失败: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(format!("git checkout 失败: {stderr}"))
    }
}

/// 工作区 git 改动列表：(未暂存, 已暂存)。非 git 仓库返回 None。
/// 口径同 ZCode gitCliRepo：porcelain 拿状态，numstat 拿增删行数，
/// untracked 逐文件读内容数行（>1MB / 含 NUL 的二进制计 0）。
pub fn git_status(cwd: &Path) -> Option<(Vec<GitFileChange>, Vec<GitFileChange>)> {
    let status_out = run_git(
        cwd,
        &["status", "--porcelain", "-z", "--untracked-files=all"],
    )?;
    let unstaged_numstat =
        run_git(cwd, &["diff", "--numstat", "-z", "--find-renames", "--"]).unwrap_or_default();
    let staged_numstat = run_git(
        cwd,
        &[
            "diff",
            "--cached",
            "--numstat",
            "-z",
            "--find-renames",
            "--",
        ],
    )
    .unwrap_or_default();
    // porcelain/numstat 的路径都相对仓库根
    let root = run_git(cwd, &["rev-parse", "--show-toplevel"])
        .map(|s| Path::new(s.trim()).to_path_buf())
        .unwrap_or_else(|| cwd.to_path_buf());

    let statuses = parse_porcelain(&status_out);
    let unstaged_stats = parse_numstat(&unstaged_numstat);
    let staged_stats = parse_numstat(&staged_numstat);

    let mut unstaged = Vec::new();
    let mut staged = Vec::new();
    for (x, y, path) in statuses {
        if x == '?' {
            unstaged.push(GitFileChange {
                additions: count_file_lines(&root.join(&path)),
                deletions: 0,
                path,
                status: "?".to_string(),
            });
            continue;
        }
        if x == 'U' || y == 'U' || (x == 'A' && y == 'A') || (x == 'D' && y == 'D') {
            // 冲突（UU/AA/DD）：行数靠 git diff 算不准，归 0 展示
            unstaged.push(GitFileChange {
                path,
                additions: 0,
                deletions: 0,
                status: "C".to_string(),
            });
            continue;
        }
        if y != ' ' {
            let (additions, deletions) = unstaged_stats.get(&path).copied().unwrap_or((0, 0));
            unstaged.push(GitFileChange {
                path: path.clone(),
                additions,
                deletions,
                status: y.to_string(),
            });
        }
        if x != ' ' {
            let (additions, deletions) = staged_stats.get(&path).copied().unwrap_or((0, 0));
            staged.push(GitFileChange {
                path,
                additions,
                deletions,
                status: x.to_string(),
            });
        }
    }
    Some((unstaged, staged))
}

/// 单文件 git diff 原文。staged=false 未暂存（untracked 手工拼 /dev/null 全新增），
/// true 已暂存（git diff --cached）。
pub fn git_diff(cwd: &Path, path: &str, staged: bool) -> String {
    if staged {
        return run_git(cwd, &["diff", "--cached", "--no-color", "--", path])
            .map(truncate_diff)
            .unwrap_or_default();
    }
    let out = run_git(cwd, &["diff", "--no-color", "--", path]).unwrap_or_default();
    if !out.trim().is_empty() {
        return truncate_diff(out);
    }
    // git diff 无输出 = 未跟踪文件（或未变更）：按 /dev/null → 全新增拼
    untracked_diff(cwd, path).unwrap_or_default()
}

fn truncate_diff(diff: String) -> String {
    if diff.len() <= DIFF_MAX_BYTES {
        return diff;
    }
    let mut end = DIFF_MAX_BYTES;
    while !diff.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… diff 超过 1MB，已截断 …", &diff[..end])
}

/// untracked 文件的合成 diff（ZCode buildUntrackedTextDiffResult 同款思路）：
/// 空 → 全文。二进制/超 1MB 给占位说明。
fn untracked_diff(cwd: &Path, path: &str) -> Option<String> {
    let root = run_git(cwd, &["rev-parse", "--show-toplevel"])
        .map(|s| Path::new(s.trim()).to_path_buf())
        .unwrap_or_else(|| cwd.to_path_buf());
    let full = root.join(path);
    let meta = std::fs::metadata(&full).ok()?;
    if meta.len() > UNTRACKED_STAT_MAX_BYTES {
        return Some(format!("（文件超过 1MB，无文本 diff）"));
    }
    let bytes = std::fs::read(&full).ok()?;
    if bytes.contains(&0) {
        return Some("（二进制文件，无文本 diff）".to_string());
    }
    let content = String::from_utf8_lossy(&bytes).into_owned();
    Some(crate::tool::per_edit_diff(&root, &full, "", &content).unified_diff)
}

/// porcelain v1 -z 解析：(X, Y, path)。重命名取新路径并跳过紧随的原始路径字段。
fn parse_porcelain(out: &str) -> Vec<(char, char, String)> {
    let mut entries = Vec::new();
    let mut fields = out.split('\0');
    while let Some(field) = fields.next() {
        if field.len() < 4 {
            continue;
        }
        let mut chars = field.chars();
        let (Some(x), Some(y)) = (chars.next(), chars.next()) else {
            continue;
        };
        let path = field[3..].to_string();
        if x == 'R' || y == 'R' || x == 'C' || y == 'C' {
            // 重命名/复制：下一个 NUL 字段是原始路径，跳过
            fields.next();
        }
        if path.is_empty() {
            continue;
        }
        entries.push((x, y, path));
    }
    entries
}

/// numstat -z 解析：path → (additions, deletions)。二进制（- -）计 (0,0)；重命名取新路径。
fn parse_numstat(out: &str) -> std::collections::HashMap<String, (u32, u32)> {
    let mut map = std::collections::HashMap::new();
    let mut fields = out.split('\0').peekable();
    while let Some(field) = fields.next() {
        let mut parts = field.splitn(3, '\t');
        let (Some(a), Some(d), Some(path)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let additions = a.parse().unwrap_or(0);
        let deletions = d.parse().unwrap_or(0);
        let path = if path.is_empty() {
            // 重命名：a\td\t\0old\0new\0
            let _old = fields.next();
            fields.next().unwrap_or_default().to_string()
        } else {
            path.to_string()
        };
        if path.is_empty() {
            continue;
        }
        map.insert(path, (additions, deletions));
    }
    map
}

/// untracked 文件行数：>1MB 或含 NUL（二进制）计 0；末尾无换行补 1 行。
fn count_file_lines(path: &Path) -> u32 {
    let Ok(meta) = std::fs::metadata(path) else {
        return 0;
    };
    if !meta.is_file() || meta.len() > UNTRACKED_STAT_MAX_BYTES {
        return 0;
    }
    let Ok(bytes) = std::fs::read(path) else {
        return 0;
    };
    if bytes.contains(&0) {
        return 0;
    }
    let mut lines = bytes.iter().filter(|&&b| b == b'\n').count() as u32;
    if !bytes.is_empty() && !bytes.ends_with(b"\n") {
        lines += 1;
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_porcelain_handles_rename_untracked_and_modified() {
        let out = "R  new.txt\0old.txt\0?? untracked.txt\0 M mod.txt\0M  staged.txt\0";
        let entries = parse_porcelain(out);
        assert_eq!(
            entries,
            vec![
                ('R', ' ', "new.txt".to_string()),
                ('?', '?', "untracked.txt".to_string()),
                (' ', 'M', "mod.txt".to_string()),
                ('M', ' ', "staged.txt".to_string()),
            ]
        );
    }

    #[test]
    fn parse_numstat_handles_rename_binary_and_normal() {
        let out = "3\t1\t\0old.txt\0new.txt\0-\t-\tbin.png\010\t2\tplain.txt\0";
        let map = parse_numstat(out);
        assert_eq!(map.get("new.txt"), Some(&(3, 1)));
        assert_eq!(map.get("bin.png"), Some(&(0, 0)));
        assert_eq!(map.get("plain.txt"), Some(&(10, 2)));
        assert_eq!(map.get("old.txt"), None, "重命名取新路径");
    }

    #[test]
    fn count_file_lines_counts_partial_last_line() {
        let dir = std::env::temp_dir().join(format!("pig-git-lines-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("f.txt");
        std::fs::write(&file, "a\nb\nc").unwrap();
        assert_eq!(count_file_lines(&file), 3);
        std::fs::write(&file, "a\nb\n").unwrap();
        assert_eq!(count_file_lines(&file), 2);
        std::fs::write(&file, [b'a', 0, b'b']).unwrap();
        assert_eq!(count_file_lines(&file), 0, "含 NUL 视为二进制");
    }
}
