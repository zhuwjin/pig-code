use crate::NoConsoleExt as _;
use std::path::Path;

use pig_protocol::{CoreError, GitDiffNote, GitFileChange};
use serde::{Deserialize, Serialize};

/// Line-count cap for untracked files (same value as ZCode's GIT_UNTRACKED_STAT_MAX_BYTES)
const UNTRACKED_STAT_MAX_BYTES: u64 = 1024 * 1024;
/// Cap for a single file's raw diff (same value as ZCode's DEFAULT_GIT_DIFF_BYTES)
const DIFF_MAX_BYTES: usize = 1024 * 1024;
/// Cap for one read-only plugin Git response.
const READ_MAX_BYTES: usize = 1024 * 1024;
const MAX_LOG_LIMIT: u32 = 100;
const DEFAULT_LOG_LIMIT: u32 = 20;

/// A commit row for `git_log`. Local (not protocol) because only the plugin
/// service consumes it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitCommit {
    pub sha: String,
    pub subject: String,
    pub timestamp: u64,
}

/// One hardened read-only Git command. Hooks, fsmonitor and external diff
/// helpers are disabled so repository configuration cannot execute code while
/// the host is only reading.
fn git_command(cwd: &Path) -> std::process::Command {
    let mut command = std::process::Command::new("git");
    command
        .no_console()
        .current_dir(cwd)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .args([
            "-c",
            if cfg!(windows) {
                "core.hooksPath=NUL"
            } else {
                "core.hooksPath=/dev/null"
            },
            "-c",
            "core.fsmonitor=false",
        ]);
    command
}

fn run_git(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = git_command(cwd)
        .args(args)
        .output()
        .ok()
        .filter(|out| out.status.success())?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn run_git_checked(cwd: &Path, args: &[&str]) -> Result<String, CoreError> {
    let out = git_command(cwd)
        .args(args)
        .output()
        .map_err(|error| CoreError::GitSpawn {
            detail: error.to_string(),
        })?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    Err(CoreError::Internal {
        detail: if stderr.is_empty() {
            format!("git {} failed", args.first().copied().unwrap_or_default())
        } else {
            stderr
        },
    })
}

fn ensure_repository(cwd: &Path) -> Result<(), CoreError> {
    run_git_checked(cwd, &["rev-parse", "--git-dir"])
        .map(|_| ())
        .map_err(|_| CoreError::PluginInvalidInput {
            detail: "the session workspace is not a Git repository".into(),
        })
}

fn cap_text(mut text: String) -> (String, bool) {
    if text.len() <= READ_MAX_BYTES {
        return (text, false);
    }
    let mut end = READ_MAX_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    (text, true)
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
    let unstaged_numstat = run_git(
        cwd,
        &[
            "diff",
            "--numstat",
            "-z",
            "--find-renames",
            "--no-ext-diff",
            "--no-textconv",
            "--",
        ],
    )
    .unwrap_or_default();
    let staged_numstat = run_git(
        cwd,
        &[
            "diff",
            "--cached",
            "--numstat",
            "-z",
            "--find-renames",
            "--no-ext-diff",
            "--no-textconv",
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
        return run_git(
            cwd,
            &[
                "diff",
                "--cached",
                "--no-color",
                "--no-ext-diff",
                "--no-textconv",
                "--",
                path,
            ],
        )
        .map(truncate_diff)
        .unwrap_or_default();
    }
    let out = run_git(
        cwd,
        &[
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--",
            path,
        ],
    )
    .unwrap_or_default();
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
    untracked_diff_at(&root, &root.join(path))
}

fn untracked_diff_at(base: &Path, full: &Path) -> Option<(String, Option<GitDiffNote>)> {
    let meta = std::fs::metadata(full).ok()?;
    if meta.len() > UNTRACKED_STAT_MAX_BYTES {
        return Some((String::new(), Some(GitDiffNote::TooLarge)));
    }
    let bytes = std::fs::read(full).ok()?;
    if bytes.contains(&0) {
        return Some((String::new(), Some(GitDiffNote::Binary)));
    }
    let content = String::from_utf8_lossy(&bytes).into_owned();
    Some((
        crate::tool::per_edit_diff(base, full, "", &content).unified_diff,
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

/// `git status` for a plugin: changed paths are made relative to the session
/// workspace, and entries outside it are dropped. This keeps a plugin session
/// opened in a repository subdirectory from reading the rest of the checkout.
pub fn git_status_read(cwd: &Path) -> Result<(Vec<GitFileChange>, Vec<GitFileChange>), CoreError> {
    ensure_repository(cwd)?;
    let (mut unstaged, mut staged) = git_status(cwd).ok_or_else(|| CoreError::Internal {
        detail: "git status returned no output".into(),
    })?;
    let root = run_git_checked(cwd, &["rev-parse", "--show-toplevel"])?;
    let root = Path::new(root.trim());
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let cwd = cwd.canonicalize().map_err(|error| CoreError::Internal {
        detail: error.to_string(),
    })?;
    let prefix = cwd
        .strip_prefix(&root)
        .ok()
        .map(|prefix| prefix.to_string_lossy().replace('\\', "/"))
        .filter(|prefix| !prefix.is_empty());
    if let Some(prefix) = prefix {
        let prefix = format!("{prefix}/");
        let scope = |changes: Vec<GitFileChange>| {
            changes
                .into_iter()
                .filter_map(|mut change| {
                    let stripped = change.path.strip_prefix(&prefix).map(str::to_owned);
                    stripped.map(|path| {
                        change.path = path;
                        change
                    })
                })
                .collect::<Vec<_>>()
        };
        unstaged = scope(unstaged);
        staged = scope(staged);
    }
    Ok((unstaged, staged))
}

/// `git diff` for one workspace-relative path. Reads are hardened by
/// `run_git` (hooks, fsmonitor and external helpers disabled).
pub fn git_diff_read(
    cwd: &Path,
    path: &str,
    staged: bool,
) -> Result<(String, Option<GitDiffNote>), CoreError> {
    validate_relative_path(path)?;
    ensure_repository(cwd)?;
    let out = if staged {
        run_git(
            cwd,
            &[
                "diff",
                "--cached",
                "--no-color",
                "--no-ext-diff",
                "--no-textconv",
                "--",
                path,
            ],
        )
    } else {
        run_git(
            cwd,
            &[
                "diff",
                "--no-color",
                "--no-ext-diff",
                "--no-textconv",
                "--",
                path,
            ],
        )
    }
    .unwrap_or_default();
    if !out.trim().is_empty() {
        return Ok(truncate_diff(out));
    }
    if !staged {
        let status = run_git(
            cwd,
            &["status", "--porcelain", "--untracked-files=all", "--", path],
        )
        .unwrap_or_default();
        if status.lines().any(|line| line.starts_with("??")) {
            let full = cwd.join(path);
            if let Some(diff) = untracked_diff_at(cwd, &full) {
                return Ok(diff);
            }
        }
    }
    Ok((String::new(), None))
}

pub fn git_log_read(
    cwd: &Path,
    limit: Option<u32>,
    path: Option<&str>,
) -> Result<Vec<GitCommit>, CoreError> {
    ensure_repository(cwd)?;
    if let Some(path) = path {
        validate_relative_path(path)?;
    }
    let limit = limit.unwrap_or(DEFAULT_LOG_LIMIT).clamp(1, MAX_LOG_LIMIT);
    let mut args = vec![
        "log".to_string(),
        format!("-{limit}"),
        "--no-color".to_string(),
        "--format=%H%x1f%s%x1f%ct%x1e".to_string(),
    ];
    // `.` keeps a session opened in a repository subdirectory from reading
    // history that only touches files outside its workspace.
    args.push("--".into());
    args.push(path.unwrap_or(".").to_owned());
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let output = run_git_checked(cwd, &refs)?;
    let mut commits = Vec::new();
    for record in output.split('\x1e') {
        let record = record.trim_matches(['\n', '\r']);
        if record.is_empty() {
            continue;
        }
        let mut fields = record.splitn(3, '\x1f');
        let (Some(sha), Some(subject), Some(timestamp)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        commits.push(GitCommit {
            sha: sha.to_owned(),
            subject: subject.chars().take(4096).collect(),
            timestamp: timestamp.trim().parse().unwrap_or(0),
        });
    }
    Ok(commits)
}

pub fn git_show_read(
    cwd: &Path,
    rev: &str,
    path: Option<&str>,
) -> Result<(String, bool), CoreError> {
    validate_rev(rev)?;
    ensure_repository(cwd)?;
    if let Some(path) = path {
        validate_relative_path(path)?;
    }
    let mut args = vec![
        "show".to_string(),
        "--no-color".to_string(),
        "--no-ext-diff".to_string(),
        "--no-textconv".to_string(),
        rev.to_owned(),
    ];
    // A commit patch is repo-wide by default; `-- .` scopes it to the session
    // workspace when the session cwd is a subdirectory.
    args.push("--".into());
    args.push(path.unwrap_or(".").to_owned());
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let output = run_git_checked(cwd, &refs)?;
    Ok(cap_text(output))
}

pub fn git_branches_read(cwd: &Path) -> Result<(Option<String>, Vec<String>), CoreError> {
    ensure_repository(cwd)?;
    let current = run_git_checked(cwd, &["rev-parse", "--abbrev-ref", "HEAD"])?
        .trim()
        .to_owned();
    let current = (!current.is_empty() && current != "HEAD").then_some(current);
    let branches = run_git_checked(
        cwd,
        &["for-each-ref", "--format=%(refname:short)", "refs/heads"],
    )?
    .lines()
    .map(str::trim)
    .filter(|line| !line.is_empty())
    .map(str::to_owned)
    .collect();
    Ok((current, branches))
}

/// The plugin API accepts only workspace-relative paths: no absolute path, no
/// `.`/`..` component, no repository metadata.
pub fn validate_relative_path(path: &str) -> Result<(), CoreError> {
    let path = Path::new(path.trim());
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || !path
            .components()
            .all(|part| matches!(part, std::path::Component::Normal(_)))
        || path.components().any(|part| match part {
            std::path::Component::Normal(name) => name
                .to_str()
                .is_some_and(|name| name.eq_ignore_ascii_case(".git")),
            _ => false,
        })
    {
        return Err(CoreError::PluginInvalidInput {
            detail: "the Git path must be a workspace-relative path without `.git`".into(),
        });
    }
    Ok(())
}

fn validate_rev(rev: &str) -> Result<(), CoreError> {
    let valid = !rev.is_empty()
        && rev.len() <= 200
        && !rev.starts_with('-')
        && rev
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._/@{}^~-".contains(&byte));
    if valid {
        Ok(())
    } else {
        Err(CoreError::PluginInvalidInput {
            detail: "the Git revision is invalid".into(),
        })
    }
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
    fn read_only_helpers_return_status_log_show_and_branches() {
        let dir = std::env::temp_dir().join(format!(
            "pig-git-read-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create repo");
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&dir)
                .env("GIT_AUTHOR_NAME", "Pig Test")
                .env("GIT_AUTHOR_EMAIL", "pig@example.test")
                .env("GIT_COMMITTER_NAME", "Pig Test")
                .env("GIT_COMMITTER_EMAIL", "pig@example.test")
                .output()
                .expect("run git");
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run(&["init", "-q"]);
        std::fs::write(dir.join("file.txt"), "one\ntwo\n").expect("write file");
        run(&["add", "file.txt"]);
        run(&["commit", "-q", "-m", "initial commit"]);
        std::fs::write(dir.join("file.txt"), "one\ntwo\nthree\n").expect("modify file");

        let (unstaged, staged) = git_status_read(&dir).expect("status");
        assert!(staged.is_empty());
        assert!(unstaged.iter().any(|change| change.path == "file.txt"));
        let (tracked_diff, _) = git_diff_read(&dir, "file.txt", false).expect("tracked diff");
        assert!(tracked_diff.contains("+three"));
        std::fs::write(dir.join("new.txt"), "new\n").expect("untracked file");
        let (untracked_diff, _) = git_diff_read(&dir, "new.txt", false).expect("untracked diff");
        assert!(untracked_diff.contains("+new"), "{untracked_diff}");
        let commits = git_log_read(&dir, Some(5), None).expect("log");
        assert_eq!(commits[0].subject, "initial commit");
        let (show, truncated) = git_show_read(&dir, "HEAD", None).expect("show");
        assert!(!truncated);
        assert!(show.contains("initial commit"));
        let (current, branches) = git_branches_read(&dir).expect("branches");
        assert!(current.is_some());
        assert!(!branches.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn status_scopes_changes_to_a_subdirectory_workspace() {
        let dir = std::env::temp_dir().join(format!(
            "pig-git-scope-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("sub")).expect("create repo");
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&dir)
                .env("GIT_AUTHOR_NAME", "Pig Test")
                .env("GIT_AUTHOR_EMAIL", "pig@example.test")
                .env("GIT_COMMITTER_NAME", "Pig Test")
                .env("GIT_COMMITTER_EMAIL", "pig@example.test")
                .output()
                .expect("run git");
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        std::fs::write(dir.join("outside.txt"), "outside\n").expect("outside");
        std::fs::write(dir.join("sub/inside.txt"), "inside\n").expect("inside");
        run(&["init", "-q"]);
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "initial"]);
        std::fs::write(dir.join("outside.txt"), "outside changed\n").expect("outside");
        std::fs::write(dir.join("sub/inside.txt"), "inside changed\n").expect("inside");
        let (unstaged, _) = git_status_read(&dir.join("sub")).expect("status");
        assert_eq!(unstaged.len(), 1, "{unstaged:?}");
        assert_eq!(unstaged[0].path, "inside.txt");
        let _ = std::fs::remove_dir_all(dir);
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
