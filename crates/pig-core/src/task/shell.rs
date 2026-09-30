use super::*;

/// Windows shell 探测结果：优先 Git Bash（Unix 语法 + UTF-8 输出），
/// 找不到回退 cmd（pig-code 的既有行为，零新增失败模式）。
#[derive(Clone, Debug)]
pub enum WindowsShell {
    GitBash(PathBuf),
    Cmd,
}

/// 进程级缓存：探测链要跑 `git --exec-path`（子进程），不该每条命令重复；
/// 结果在进程生命周期内不变，提示词 env 块也复用它做 Shell 标注。
pub(crate) static WINDOWS_SHELL: std::sync::OnceLock<WindowsShell> = std::sync::OnceLock::new();

pub fn windows_shell() -> WindowsShell {
    WINDOWS_SHELL.get_or_init(detect_windows_shell).clone()
}

pub(crate) fn detect_windows_shell() -> WindowsShell {
    detect_git_bash().map_or(WindowsShell::Cmd, WindowsShell::GitBash)
}

/// Git Bash 探测链（kimi-code 同款）：
/// PIGCODE_SHELL_PATH 显式指定 → PATH 上的 bash.exe → PATH 上的 git.exe 反推
/// 安装根（常规 cmd/bin 布局取上级；包管理器 shim 用 `git --exec-path` 穿透）
/// → 常规安装位置。
pub(crate) fn detect_git_bash() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("PIGCODE_SHELL_PATH") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }
    let dirs: Vec<PathBuf> =
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect();
    for dir in &dirs {
        let candidate = dir.join("bash.exe");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    for dir in &dirs {
        for git_exe in [
            dir.join("git.exe"),
            dir.join("cmd").join("git.exe"),
            dir.join("bin").join("git.exe"),
        ] {
            if !git_exe.is_file() {
                continue;
            }
            for candidate in git_bash_candidates(&git_exe) {
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
            if let Some(root) = git_root_from_exec_path(&git_exe) {
                for sub in ["bin", "usr\\bin"] {
                    let candidate = root.join(sub).join("bash.exe");
                    if candidate.is_file() {
                        return Some(candidate);
                    }
                }
            }
        }
    }
    for base in program_file_bases() {
        for sub in ["Git\\bin", "Git\\usr\\bin"] {
            let candidate = base.join(sub).join("bash.exe");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// git.exe → 同根 bash.exe 候选（常规安装布局：git 在 cmd\ 或 bin\ 下，取上上级为根）。
pub(crate) fn git_bash_candidates(git_exe: &Path) -> Vec<PathBuf> {
    let Some(parent) = git_exe.parent() else {
        return Vec::new();
    };
    let in_layout_dir = parent
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|name| matches!(name.to_ascii_lowercase().as_str(), "cmd" | "bin"));
    if !in_layout_dir {
        return Vec::new();
    }
    let Some(root) = parent.parent() else {
        return Vec::new();
    };
    vec![
        root.join("bin").join("bash.exe"),
        root.join("usr").join("bash.exe"),
    ]
}

/// `git --exec-path` 输出 → 安装根：…/Git/mingw64/libexec/git-core → …/Git。
/// Scoop/Chocolatey/WinGet 的 shim 不在 cmd/bin 布局里，靠这一步定位真实安装根。
pub(crate) fn git_root_from_exec_path(git_exe: &Path) -> Option<PathBuf> {
    let output = std::process::Command::new(git_exe)
        .no_console()
        .arg("--exec-path")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    root_from_exec_path_text(&String::from_utf8_lossy(&output.stdout))
}

pub(crate) fn root_from_exec_path_text(text: &str) -> Option<PathBuf> {
    let path = Path::new(text.trim());
    let components: Vec<_> = path.components().collect();
    for (ix, component) in components.iter().enumerate() {
        if let std::path::Component::Normal(name) = component {
            let Some(name) = name.to_str() else { continue };
            if matches!(
                name.to_ascii_lowercase().as_str(),
                "mingw32" | "mingw64" | "ucrt64" | "clang64" | "clangarm64"
            ) {
                return Some(components[..ix].iter().collect());
            }
        }
    }
    // 无 MINGW 段（非常规布局）：exec-path/libexec/git-core 往上两级兜底
    path.ancestors().nth(2).map(Path::to_path_buf)
}

pub(crate) fn program_file_bases() -> Vec<PathBuf> {
    let mut bases = Vec::new();
    for key in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(value) = std::env::var_os(key) {
            bases.push(PathBuf::from(value));
        }
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        bases.push(PathBuf::from(local).join("Programs"));
    }
    bases
}

/// 提示词 env 块的 Shell 标注：模型据此选择命令方言。
pub fn shell_label() -> String {
    if cfg!(target_os = "windows") {
        match windows_shell() {
            WindowsShell::GitBash(_) => "Git Bash（bash -c，Unix 语法）".to_string(),
            WindowsShell::Cmd => "cmd /C（Windows 语法）".to_string(),
        }
    } else {
        "sh -c（Unix 语法）".to_string()
    }
}

/// cmd 的 NUL 重定向改写为 /dev/null（Git Bash 下 NUL 设备不可用；kimi 同款）。
/// 只改写 `>nul`/`>NUL`（含 `>>` 与空格形态），不影响作为普通参数的 NUL。
pub(crate) fn rewrite_nul_redirects(command: &str) -> String {
    command
        .replace("> nul", "> /dev/null")
        .replace("> NUL", "> /dev/null")
        .replace(">nul", ">/dev/null")
        .replace(">NUL", ">/dev/null")
}

/// 统一起 shell：Windows 优先 Git Bash（bash -c，探测见 windows_shell）、
/// 回退 cmd /C；其余平台 sh -c。工作目录、stdin null、stdout/stderr piped、
/// kill_on_drop。注入 NO_COLOR=1 / TERM=dumb / GIT_TERMINAL_PROMPT=0
///（防 git 交互提问挂死）+ PYTHONIOENCODING/PYTHONUTF8=1（Python 子进程强制
/// UTF-8 输出，ZCode 同款）；LANG 未设时补 C.UTF-8。
/// unix 上 process_group(0) 让子进程自成进程组组长，stop_task 才能整组树杀。
pub(crate) fn spawn_shell(cwd: &Path, command: &str) -> std::io::Result<tokio::process::Child> {
    let command = command.to_string();
    let mut shell = if cfg!(target_os = "windows") {
        match windows_shell() {
            WindowsShell::GitBash(bash) => {
                let mut shell = tokio::process::Command::new(bash);
                shell.arg("-c").arg(rewrite_nul_redirects(&command));
                shell
            }
            WindowsShell::Cmd => {
                let mut shell = tokio::process::Command::new("cmd");
                shell.arg("/C").arg(command);
                shell
            }
        }
    } else {
        let mut shell = tokio::process::Command::new("sh");
        shell.arg("-c").arg(command);
        shell
    };
    shell
        .no_console()
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .env("NO_COLOR", "1")
        .env("TERM", "dumb")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("PYTHONIOENCODING", "utf-8")
        .env("PYTHONUTF8", "1");
    if std::env::var_os("LANG").is_none() && std::env::var_os("LC_ALL").is_none() {
        shell.env("LANG", "C.UTF-8");
    }
    shell.kill_on_drop(true);
    #[cfg(unix)]
    shell.process_group(0);
    shell.spawn()
}
