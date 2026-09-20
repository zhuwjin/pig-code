use pig_protocol::ExecMode;

/// 读取全局（{data_dir}/AGENTS.md）+ 项目（{cwd}/AGENTS.md）指令，总量 32KB 截断。
pub fn agents_md(data_dir: &std::path::Path, cwd: &std::path::Path) -> String {
    let mut out = String::new();
    for (label, path) in [
        ("全局", data_dir.join("AGENTS.md")),
        ("项目", cwd.join("AGENTS.md")),
    ] {
        if let Ok(content) = std::fs::read_to_string(&path) {
            out.push_str(&format!("
## {label} AGENTS.md

{content}
"));
        }
        if out.len() > 32 * 1024 {
            out.truncate(32 * 1024);
            out.push_str("
[AGENTS.md 过长，已截断]");
            break;
        }
    }
    out
}

pub fn system_prompt(
    cwd: &std::path::Path,
    has_tools: bool,
    mode: ExecMode,
    data_dir: &std::path::Path,
) -> String {
    let mut prompt = format!(
        "你是 pig-code，一个运行在用户工作区里的 AI 编程助手。\n\n\
         <env>\n\
         工作目录: {}\n\
         平台: {}-{}\n\
         日期: {}\n\
         {}\
         </env>\n\n\
         行为准则:\n\
         - 回答使用与用户相同的语言（默认中文）。\n\
         - 修改代码前先读文件确认现状，不要臆测文件内容。\n\
         - 不执行有破坏性的命令（删除、格式化、强制推送等）。\n\
         - 读文件/搜索优先用 read_file、glob、grep 专用工具，而非 bash。\n\
         - 回答简洁，代码用 Markdown 代码块给出。\n",
        cwd.display(),
        std::env::consts::OS,
        std::env::consts::ARCH,
        today(),
        git_info(cwd).map(|info| format!("git: {info}\n")).unwrap_or_default(),
    );
    prompt.push_str(match mode {
        ExecMode::ConfirmBeforeEdit => {
            "\n当前执行模式: 变更前确认。修改文件或执行命令前会先请用户审批，审批通过才会执行。\n"
        }
        ExecMode::AutoEdit => {
            "\n当前执行模式: 自动编辑。可以直接修改文件；执行命令前会先请用户审批。\n"
        }
        ExecMode::Plan => {
            "\n当前执行模式: 计划模式。你是只读的：不要调用 write_file/edit/bash 等修改类工具，\
             只能用 read_file/glob/grep 调研，最终输出一份可执行的计划文本。\n"
        }
        ExecMode::FullAccess => {
            "\n当前执行模式: 完全访问。所有工具直接执行，无需审批；仍禁止破坏性命令。\n"
        }
    });
    let agents = agents_md(data_dir, cwd);
    if !agents.is_empty() {
        prompt.push_str(&agents);
    }
    if has_tools {
        prompt.push_str(
            "\n可用工具:\n\
             - read_file: 读取工作区文件内容（path 相对工作目录，支持 offset/limit 分页）。\n\
             - write_file: 写入整个文件（自动创建父目录）。\n\
             - edit: 精确替换文件文本（old_string 必须唯一出现）。\n\
             - glob: 按模式匹配文件名（如 **/*.rs）。\n\
             - grep: 正则搜索文件内容，输出 文件:行号: 内容。\n\
             - bash: 执行 shell 命令（Windows 下为 cmd /C），返回 stdout/stderr 与退出码。\n\n\
             需要了解文件内容或验证改动时主动调用工具，拿到结果后再回答。\n",
        );
    }
    prompt
}


/// 今天日期（YYYY-MM-DD，UTC）。std 无日期格式化，用 civil-from-days 算法。
fn today() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// git 分支与 dirty 状态；非 git 仓库或命令失败返回 None。
fn git_info(cwd: &std::path::Path) -> Option<String> {
    let branch = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !branch.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&branch.stdout).trim().to_string();
    if branch.is_empty() {
        return None;
    }
    let dirty = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(cwd)
        .output()
        .map(|out| !out.stdout.is_empty())
        .unwrap_or(false);
    Some(format!("{branch}{}", if dirty { " (有未提交变更)" } else { "" }))
}
