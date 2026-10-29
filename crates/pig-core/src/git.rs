use crate::NoConsoleExt as _;
use std::path::Path;

use pig_protocol::{CoreError, GitDiffNote, GitFileChange};

/// Line-count cap for untracked files (same value as ZCode's GIT_UNTRACKED_STAT_MAX_BYTES)
const UNTRACKED_STAT_MAX_BYTES: u64 = 1024 * 1024;
/// Cap for a single file's raw diff (same value as ZCode's DEFAULT_GIT_DIFF_BYTES)
const DIFF_MAX_BYTES: usize = 1024 * 1024;

fn run_git(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .no_console()
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()
        .filter(|out| out.status.success())?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Query a directory's git info: current branch + local branch list. Non-git directories return (None, vec![]).
pub fn git_info(cwd: &Path) -> (Option<String>, Vec<String>) {
    let current = std::process::Command::new("git")
        .no_console()
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(cwd)
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|name| !name.is_empty());

    let branches = std::process::Command::new("git")
        .no_console()
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

/// Switch branches; failure forwards git stderr.
pub fn checkout(cwd: &Path, branch: &str) -> Result<(), CoreError> {
    let output = std::process::Command::new("git")
        .no_console()
        .args(["checkout", branch])
        .current_dir(cwd)
        .output()
        .map_err(|e| CoreError::GitSpawn {
            detail: e.to_string(),
        })?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(CoreError::GitCheckout { detail: stderr })
    }
}

/// Workspace git change list: (unstaged, staged). Non-git directories return None.
/// Same criteria as ZCode's gitCliRepo: porcelain for status, numstat for added/removed lines,
/// untracked files read and line-counted per file (>1MB / binary with NUL counts as 0).
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
    // porcelain/numstat paths are both relative to the repository root
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
            // Conflict (UU/AA/DD): line counts via git diff are unreliable; shown as 0
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

/// Single-file raw git diff + structured placeholder note. staged=false means unstaged (untracked files
/// get a hand-built /dev/null all-added diff), true means staged (git diff --cached).
/// note = Some: diff unavailable/truncated (the UI localizes the placeholder text by kind); the diff field is truncated text or empty.
pub fn git_diff(cwd: &Path, path: &str, staged: bool) -> (String, Option<GitDiffNote>) {
    if staged {
        return run_git(cwd, &["diff", "--cached", "--no-color", "--", path])
            .map(truncate_diff)
            .unwrap_or_default();
    }
    let out = run_git(cwd, &["diff", "--no-color", "--", path]).unwrap_or_default();
    if !out.trim().is_empty() {
        return truncate_diff(out);
    }
    // Empty git diff output = untracked file (or unchanged): build it as /dev/null -> all-added
    untracked_diff(cwd, path).unwrap_or_default()
}

fn truncate_diff(diff: String) -> (String, Option<GitDiffNote>) {
    if diff.len() <= DIFF_MAX_BYTES {
        return (diff, None);
    }
    let mut end = DIFF_MAX_BYTES;
    while !diff.is_char_boundary(end) {
        end -= 1;
    }
    (diff[..end].to_string(), Some(GitDiffNote::Truncated))
}

/// Synthesized diff for untracked files (same idea as ZCode's buildUntrackedTextDiffResult):
/// empty -> full text. Binary/over-1MB yields no text diff (note placeholder).
fn untracked_diff(cwd: &Path, path: &str) -> Option<(String, Option<GitDiffNote>)> {
    let root = run_git(cwd, &["rev-parse", "--show-toplevel"])
        .map(|s| Path::new(s.trim()).to_path_buf())
        .unwrap_or_else(|| cwd.to_path_buf());
    let full = root.join(path);
    let meta = std::fs::metadata(&full).ok()?;
    if meta.len() > UNTRACKED_STAT_MAX_BYTES {
        return Some((String::new(), Some(GitDiffNote::TooLarge)));
    }
    let bytes = std::fs::read(&full).ok()?;
    if bytes.contains(&0) {
        return Some((String::new(), Some(GitDiffNote::Binary)));
    }
    let content = String::from_utf8_lossy(&bytes).into_owned();
    Some((
        crate::tool::per_edit_diff(&root, &full, "", &content).unified_diff,
        None,
    ))
}

/// porcelain v1 -z parsing: (X, Y, path). Renames take the new path and skip the immediately following original-path field.
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
            // Rename/copy: the next NUL field is the original path; skip it
            fields.next();
        }
        if path.is_empty() {
            continue;
        }
        entries.push((x, y, path));
    }
    entries
}

/// numstat -z parsing: path -> (additions, deletions). Binary (- -) counts as (0,0); renames take the new path.
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
            // Rename: a\td\t\0old\0new\0
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

/// Untracked file line count: >1MB or containing NUL (binary) counts as 0; a missing final line ending adds 1 line.
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
        let out = "3\t1\t\0old.txt\0new.txt\0-\t-\tbin.png\x0010\t2\tplain.txt\0";
        let map = parse_numstat(out);
        assert_eq!(map.get("new.txt"), Some(&(3, 1)));
        assert_eq!(map.get("bin.png"), Some(&(0, 0)));
        assert_eq!(map.get("plain.txt"), Some(&(10, 2)));
        assert_eq!(map.get("old.txt"), None, "renames map to the new path");
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
        assert_eq!(count_file_lines(&file), 0, "NUL bytes mean binary");
    }
}
