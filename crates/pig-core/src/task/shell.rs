use super::*;

/// Windows shell detection result: prefer Git Bash (Unix syntax + UTF-8
/// output); fall back to cmd when not found (existing Pig Code behavior, zero
/// new failure modes).
#[derive(Clone, Debug)]
pub enum WindowsShell {
    GitBash(PathBuf),
    Cmd,
}

/// Process-level cache: the detection chain runs `git --exec-path` (a
/// subprocess) and should not be repeated for every command; the result never
/// changes within the process lifetime, and the prompt env block reuses it for
/// the Shell label.
pub(crate) static WINDOWS_SHELL: std::sync::OnceLock<WindowsShell> = std::sync::OnceLock::new();

pub fn windows_shell() -> WindowsShell {
    WINDOWS_SHELL.get_or_init(detect_windows_shell).clone()
}

pub(crate) fn detect_windows_shell() -> WindowsShell {
    detect_git_bash().map_or(WindowsShell::Cmd, WindowsShell::GitBash)
}

/// Unix shell detection (same as kimi-code environmentProbe): prefer native
/// bash — bashisms (`[[ ]]`/arrays/process substitution) fail outright under
/// dash-style sh, and behavior should not depend on the user's distro; fall
/// back to sh when none of the candidate paths is found. Process-level cache
/// (stat results never change within the process).
pub(crate) fn unix_shell() -> &'static str {
    static UNIX_SHELL: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();
    UNIX_SHELL.get_or_init(|| {
        ["/bin/bash", "/usr/bin/bash", "/usr/local/bin/bash"]
            .into_iter()
            .find(|candidate| Path::new(candidate).is_file())
            .unwrap_or("sh")
    })
}

/// Git Bash detection chain (same as kimi-code):
/// explicit PIGCODE_SHELL_PATH → bash.exe on PATH → git.exe on PATH to infer
/// the install root (parent for the regular cmd/bin layout; package-manager
/// shims are seen through via `git --exec-path`) → common install locations.
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

/// git.exe → same-root bash.exe candidates (regular install layout: git sits under cmd\ or bin\, take the grandparent as the root).
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

/// `git --exec-path` output → install root: .../Git/mingw64/libexec/git-core → .../Git.
/// Scoop/Chocolatey/WinGet shims do not follow the cmd/bin layout; this step
/// locates the real install root.
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
    // No MINGW segment (unusual layout): fall back two levels up from exec-path/libexec/git-core
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

/// Shell label for the prompt env block: the model picks the command dialect based on it.
pub fn shell_label() -> String {
    if cfg!(target_os = "windows") {
        match windows_shell() {
            WindowsShell::GitBash(_) => "Git Bash (bash -c, Unix syntax)".to_string(),
            WindowsShell::Cmd => "cmd /C (Windows syntax)".to_string(),
        }
    } else {
        format!("{} -c (Unix syntax)", unix_shell())
    }
}

/// Rewrite cmd's NUL redirects to /dev/null (the NUL device is unavailable
/// under Git Bash; same as kimi). Only rewrites `>nul`/`>NUL` (including `>>`
/// and spaced forms); NUL as a plain argument is untouched.
pub(crate) fn rewrite_nul_redirects(command: &str) -> String {
    command
        .replace("> nul", "> /dev/null")
        .replace("> NUL", "> /dev/null")
        .replace(">nul", ">/dev/null")
        .replace(">NUL", ">/dev/null")
}

/// Unified shell spawn: Windows prefers Git Bash (bash -c, detection in
/// windows_shell) and falls back to cmd /C; Unix prefers native bash and falls
/// back to sh (detection in unix_shell). Working directory, stdin null,
/// stdout/stderr piped, kill_on_drop. Injects NO_COLOR=1 / TERM=dumb /
/// GIT_TERMINAL_PROMPT=0 (prevents git interactive prompts from hanging) +
/// PYTHONIOENCODING/PYTHONUTF8=1 (forces UTF-8 output from Python subprocesses,
/// same as ZCode); adds C.UTF-8 for LANG when unset. On unix, process_group(0)
/// makes the child its own process group leader so stop_task can kill the whole
/// tree.
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
        let mut shell = tokio::process::Command::new(unix_shell());
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
