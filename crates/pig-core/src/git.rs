use std::path::Path;

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
