use pig_protocol::ExecMode;

/// 读取全局（{data_dir}/AGENTS.md）+ 工作区（{cwd}/AGENTS.md）指令，总量 32KB 截断。
pub fn agents_md(data_dir: &std::path::Path, cwd: &std::path::Path) -> String {
    let mut out = String::new();
    for (label, path) in [
        ("全局", data_dir.join("AGENTS.md")),
        ("工作区", cwd.join("AGENTS.md")),
    ] {
        if let Ok(content) = std::fs::read_to_string(&path) {
            out.push_str(&format!(
                "
## {label} AGENTS.md

{content}
"
            ));
        }
        if out.len() > 32 * 1024 {
            out.truncate(32 * 1024);
            out.push_str(
                "
[AGENTS.md 过长，已截断]",
            );
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
         - 读文件/搜索优先用 Read、Glob、Grep 专用工具，而非 Bash。\n\
         - 多步任务先用 TodoList 拆分并随时更新进度。\n\
         - 长时命令（dev server/watch/长构建）用 Bash 的 run_in_background，配合 TaskOutput 查输出。\n\
         - 需要用户拍板时用 AskUserQuestion 给出选项，而不是纯文本提问。\n\
         - 回答简洁，代码用 Markdown 代码块给出。\n",
        cwd.display(),
        std::env::consts::OS,
        std::env::consts::ARCH,
        today(),
        git_info(cwd)
            .map(|info| format!("git: {info}\n"))
            .unwrap_or_default(),
    );
    prompt.push_str(match mode {
        ExecMode::ConfirmBeforeEdit => {
            "\n当前执行模式: 变更前确认。修改文件或执行命令前会先请用户审批，审批通过才会执行。\n"
        }
        ExecMode::AutoEdit => {
            "\n当前执行模式: 自动编辑。可以直接修改文件；执行命令前会先请用户审批。\n"
        }
        ExecMode::Plan => {
            "\n当前执行模式: 计划模式。你是只读的：不要调用 Write/Edit/Bash 等修改类工具，\
             只能用 Read/Glob/Grep 调研，最终输出一份可执行的计划文本。\n"
        }
        ExecMode::FullAccess => {
            "\n当前执行模式: 完全访问。所有工具直接执行，无需审批；命中高风险命令时会弹窗请用户确认。\n"
        }
        ExecMode::Yolo => {
            "\n当前执行模式: 无管制（Yolo）。所有工具直接执行，无审批也无危险命令拦截；敏感文件（.env/私钥/凭据）仍然不可读写。\n"
        }
    });
    let agents = agents_md(data_dir, cwd);
    if !agents.is_empty() {
        prompt.push_str(&agents);
    }
    if has_tools {
        prompt.push_str(
            "\n可用工具:\n\
             - Read: 读取工作区文件（输出带行号；UTF-16/GBK 自动转码、二进制拒绝；单次约 10 万字符上限，用 offset/limit 分页）。\n\
             - Write: 写入整个文件（自动创建父目录；已存在的文件保留原编码与行尾）。\n\
             - Edit: 精确替换文件文本（old_string 须唯一出现，replace_all=true 替换全部；保留原编码与行尾）。\n\
             - Glob: 按模式匹配文件名（如 **/*.rs；尊重 .gitignore、含隐藏文件，按最近修改排序）。\n\
             - Grep: 正则搜索文件内容，输出 文件:行号: 内容（尊重 .gitignore、含隐藏文件、跳过敏感文件；ignore_case 可忽略大小写）。\n\
             - Bash: 执行 shell 命令（Windows 下为 cmd /C），返回 stdout/stderr 与退出码；支持 timeout（默认 60s/最大 300s，超时自动转后台），输出过长落盘到 .pigcode/tool-results/。\n\
             - TodoList: 管理会话级待办清单（省略参数读取，提供 todos 整体替换）。\n\
             - FetchURL: 抓取公开网页并提取正文（不支持需登录页面）。\n\
             - TaskList: 列出后台 Bash 任务（id、状态、耗时）。\n\
             - TaskOutput: 查看后台任务输出（尾部节选）。\n\
             - TaskStop: 停止仍在运行的后台任务。\n\
             - AskUserQuestion: 需要用户决策时给出 1-4 个结构化问题（每题 2-4 选项）让用户选择。\n\n\
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
    Some(format!(
        "{branch}{}",
        if dirty { " (有未提交变更)" } else { "" }
    ))
}
