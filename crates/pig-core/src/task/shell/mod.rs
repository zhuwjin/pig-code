use super::*;

use pig_protocol::HookShell;

#[cfg(unix)]
mod bash;
#[cfg(windows)]
mod powershell;

/// A concrete shell selected for one model command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelShell {
    pub kind: ShellKind,
    pub program: PathBuf,
    /// Unix only: run with `-lc` (login shell, loads the profile) instead of
    /// `-c`. Windows shells ignore it — PowerShell always runs `-NoProfile`.
    pub login: bool,
}

/// Only the variants of the host platform are constructed in one build; the
/// full set keeps the label/formatting tests and cross-platform reasoning in
/// one place.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShellKind {
    Bash,
    Zsh,
    Sh,
    PowerShell,
    Cmd,
}

impl ShellKind {
    /// Canonical model-facing name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Bash => "bash",
            Self::Zsh => "zsh",
            Self::Sh => "sh",
            Self::PowerShell => "powershell",
            Self::Cmd => "cmd",
        }
    }
}

impl ModelShell {
    /// Model-facing label for the prompt's `Shell:` line: name, version (when
    /// probing succeeded), invocation shape and dialect.
    pub fn label(&self, version: Option<&str>) -> String {
        let version = version.map(|value| format!(" {value}")).unwrap_or_default();
        match self.kind {
            ShellKind::Bash | ShellKind::Zsh | ShellKind::Sh => {
                let flag = if self.login { "-lc" } else { "-c" };
                let login = if self.login { ", login shell" } else { "" };
                format!("{}{version} {flag} (Unix syntax{login})", self.kind.name())
            }
            ShellKind::PowerShell => {
                format!("PowerShell{version} (powershell -Command, PowerShell syntax)")
            }
            ShellKind::Cmd => format!("cmd{version} (cmd /C, Windows syntax)"),
        }
    }

    fn probe_version(&self) -> Option<String> {
        #[cfg(unix)]
        {
            bash::version(self)
        }
        #[cfg(windows)]
        {
            powershell::version(self)
        }
    }

    #[cfg_attr(unix, allow(unused_variables))]
    fn command_args(&self, workdir: &Path, command: &str) -> Vec<std::ffi::OsString> {
        #[cfg(unix)]
        {
            bash::command_args(self, command)
        }
        #[cfg(windows)]
        {
            powershell::command_args(self, workdir, command)
        }
    }
}

/// Resolve the shell for one model command. `None` keeps the session default;
/// an explicit name must be on the platform allowlist and installed.
pub fn resolve_model_shell(
    request: Option<&str>,
    login: Option<bool>,
) -> Result<ModelShell, String> {
    let login = login.unwrap_or(false);
    #[cfg(unix)]
    {
        bash::resolve(request, login)
    }
    #[cfg(windows)]
    {
        powershell::resolve(request, login)
    }
}

/// The session default shell, also shown on the prompt's `Shell:` line.
pub fn default_model_shell() -> ModelShell {
    #[cfg(unix)]
    {
        bash::default_shell()
    }
    #[cfg(windows)]
    {
        powershell::default_shell()
    }
}

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

/// Git Bash detection chain (same shape as ZCode's):
/// explicit PIGCODE_SHELL_PATH → git.exe on PATH to infer the install root
/// (parent for the regular cmd/bin layout; package-manager shims are seen
/// through via `git --exec-path`) → common install locations. Deliberately
/// never scans PATH for a bare bash.exe: `C:\Windows\System32\bash.exe` sits
/// near the front of PATH and is the WSL launcher, not Git Bash — commands
/// would run inside a Linux VM (Windows tools like dotnet unreachable), and
/// the launcher re-expands the `-c` payload through an outer WSL shell, so
/// assignments/`$(...)` in one command break (seen in the wild 2026-10-08 on
/// a scoop-git machine; kimi-code's chain has the same latent trap).
pub(crate) fn detect_git_bash() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("PIGCODE_SHELL_PATH") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }
    let dirs: Vec<PathBuf> =
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect();
    git_bash_from_dirs(&dirs, &program_file_bases(), &|p| p.is_file())
}

/// Testable core of detect_git_bash (env access stays in the caller):
/// git.exe on the given dir list → install-root inference → Program
/// Files-style base dirs.
pub(crate) fn git_bash_from_dirs(
    dirs: &[PathBuf],
    bases: &[PathBuf],
    is_file: &dyn Fn(&Path) -> bool,
) -> Option<PathBuf> {
    for dir in dirs {
        for git_exe in [
            dir.join("git.exe"),
            dir.join("cmd").join("git.exe"),
            dir.join("bin").join("git.exe"),
        ] {
            if !is_file(&git_exe) {
                continue;
            }
            for candidate in git_bash_candidates(&git_exe) {
                if is_file(&candidate) {
                    return Some(candidate);
                }
            }
            if let Some(root) = git_root_from_exec_path(&git_exe) {
                for sub in ["bin", "usr\\bin"] {
                    let candidate = root.join(sub).join("bash.exe");
                    if is_file(&candidate) {
                        return Some(candidate);
                    }
                }
            }
        }
    }
    for base in bases {
        for sub in ["Git\\bin", "Git\\usr\\bin"] {
            let candidate = base.join(sub).join("bash.exe");
            if is_file(&candidate) {
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

/// Shell used for **sandboxed** model commands on Windows.
///
/// The PowerShell chain, matching Codex. It is the only shell that can run
/// inside the PSEC sandbox: the AppContainer token cannot create the
/// `\BaseNamedObjects` shared memory MSYS2 needs
/// (`NtCreateDirectoryObject` → `STATUS_ACCESS_DENIED`; microsoft/mxc#1061,
/// openai/codex#54449), and MSYS2 keeps producing sandbox failures upstream
/// (openai/codex#15016 signal pipes, #41783 wedged `git.exe`). Hooks and the
/// embedded terminal are unaffected: they run outside the sandbox and keep
/// their own Git Bash behavior.
#[derive(Clone, Debug)]
pub enum WindowsSandboxShell {
    PowerShell(PathBuf),
    Cmd,
}

/// Filesystem detection is cached: it stats PATH, the registry-derived Program
/// Files bases and the Git install.
static DETECTED_POWERSHELL: std::sync::OnceLock<WindowsSandboxShell> = std::sync::OnceLock::new();

pub fn windows_sandbox_shell() -> WindowsSandboxShell {
    DETECTED_POWERSHELL
        .get_or_init(detect_powershell_or_cmd)
        .clone()
}

/// First usable PowerShell outside `WindowsApps` (Store builds cannot run
/// inside the sandbox): `pwsh` on PATH, the well-known pwsh 7 location, then
/// Windows PowerShell under SystemRoot / PATH.
pub(crate) fn find_powershell() -> Option<PathBuf> {
    let dirs: Vec<PathBuf> =
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect();
    let mut pwsh_candidates: Vec<PathBuf> = dirs.iter().map(|dir| dir.join("pwsh.exe")).collect();
    pwsh_candidates.push(PathBuf::from(r"C:\Program Files\PowerShell\7\pwsh.exe"));
    for candidate in pwsh_candidates {
        if candidate.is_file() && !is_windows_apps_path(&candidate) {
            return Some(candidate);
        }
    }
    let mut powershell_candidates: Vec<PathBuf> = Vec::new();
    if let Some(root) = std::env::var_os("SystemRoot") {
        powershell_candidates.push(
            PathBuf::from(root)
                .join("System32")
                .join("WindowsPowerShell")
                .join("v1.0")
                .join("powershell.exe"),
        );
    }
    powershell_candidates.extend(dirs.iter().map(|dir| dir.join("powershell.exe")));
    powershell_candidates
        .into_iter()
        .find(|candidate| candidate.is_file() && !is_windows_apps_path(candidate))
}

fn detect_powershell_or_cmd() -> WindowsSandboxShell {
    find_powershell()
        .map(WindowsSandboxShell::PowerShell)
        .unwrap_or(WindowsSandboxShell::Cmd)
}

/// Store-packaged shells live under `...\WindowsApps\...` and cannot run inside
/// the sandbox.
fn is_windows_apps_path(path: &Path) -> bool {
    path.components().any(|component| {
        component
            .as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case("WindowsApps")
    })
}

/// Shell label for the prompt env block: the model picks the command dialect
/// based on it. The version probe runs once per process (a shell startup is
/// the expensive part); a failed probe simply omits the version.
pub fn shell_label() -> String {
    static LABEL: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    LABEL
        .get_or_init(|| {
            let shell = default_model_shell();
            let version = shell.probe_version();
            shell.label(version.as_deref())
        })
        .clone()
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

/// Every hook shell installed on this host, `auto` first.
///
/// The settings UI renders this list so an entry cannot select a shell that
/// would immediately fail. A synced config naming a shell that is no longer
/// installed keeps its value and fails at run time with a named error.
pub fn available_hook_shells() -> Vec<HookShell> {
    let mut out = vec![HookShell::Auto];
    for choice in platform_hook_shells() {
        if hook_shell_program(*choice).is_ok() {
            out.push(*choice);
        }
    }
    out
}

fn platform_hook_shells() -> &'static [HookShell] {
    #[cfg(windows)]
    {
        &[
            HookShell::GitBash,
            HookShell::Cmd,
            HookShell::PowerShell,
            HookShell::Pwsh,
        ]
    }
    #[cfg(unix)]
    {
        &[HookShell::Bash, HookShell::Sh, HookShell::Zsh]
    }
}

/// Resolve an explicit hook shell to its program, or explain why it is not
/// usable here. `auto` never reaches this function.
fn hook_shell_program(shell: HookShell) -> Result<PathBuf, String> {
    match shell {
        HookShell::Auto => Err("`auto` is resolved from the host shell chain".to_string()),
        #[cfg(windows)]
        HookShell::GitBash => match windows_shell() {
            WindowsShell::GitBash(bash) => Ok(bash),
            WindowsShell::Cmd => Err(unavailable(shell)),
        },
        #[cfg(windows)]
        HookShell::Cmd => Ok(cmd_program()),
        #[cfg(windows)]
        HookShell::PowerShell => find_windows_powershell().ok_or_else(|| unavailable(shell)),
        #[cfg(windows)]
        HookShell::Pwsh => find_pwsh().ok_or_else(|| unavailable(shell)),
        #[cfg(unix)]
        HookShell::Bash => bash::program_for(ShellKind::Bash).ok_or_else(|| unavailable(shell)),
        #[cfg(unix)]
        HookShell::Sh => bash::program_for(ShellKind::Sh).ok_or_else(|| unavailable(shell)),
        #[cfg(unix)]
        HookShell::Zsh => bash::program_for(ShellKind::Zsh).ok_or_else(|| unavailable(shell)),
        #[allow(unreachable_patterns)]
        other => Err(format!(
            "the hook shell `{}` is not available on this platform",
            other.name()
        )),
    }
}

fn unavailable(shell: HookShell) -> String {
    format!(
        "the configured hook shell `{}` is not installed on this host; choose another shell or `auto`",
        shell.name()
    )
}

#[cfg(windows)]
fn cmd_program() -> PathBuf {
    std::env::var_os("ComSpec")
        .map(PathBuf::from)
        .filter(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from("cmd.exe"))
}

#[cfg(windows)]
fn path_dirs() -> Vec<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect()
}

#[cfg(windows)]
fn find_windows_powershell() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(root) = std::env::var_os("SystemRoot") {
        candidates.push(
            PathBuf::from(root)
                .join("System32")
                .join("WindowsPowerShell")
                .join("v1.0")
                .join("powershell.exe"),
        );
    }
    candidates.extend(
        path_dirs()
            .into_iter()
            .map(|dir| dir.join("powershell.exe")),
    );
    candidates
        .into_iter()
        .find(|candidate| candidate.is_file() && !is_windows_apps_path(candidate))
}

#[cfg(windows)]
fn find_pwsh() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = path_dirs()
        .into_iter()
        .map(|dir| dir.join("pwsh.exe"))
        .collect();
    candidates.push(PathBuf::from(r"C:\Program Files\PowerShell\7\pwsh.exe"));
    candidates.into_iter().find(|candidate| candidate.is_file())
}

/// Unified shell preparation for hooks (never sandboxed). `auto` keeps the
/// host chain (Windows Git Bash → cmd, Unix bash → sh); an explicit choice runs
/// that installed shell and fails when it is missing. Working directory, env
/// hygiene and kill_on_drop are set here; stdio is left to the caller.
pub(crate) fn prepare_shell(
    cwd: &Path,
    command: &str,
    shell: HookShell,
) -> Result<tokio::process::Command, String> {
    Ok(prepare_process(cwd, &hook_shell_command(command, shell)?))
}

/// Strip the Windows `\\?\` verbatim prefix. Windows-only: only the PowerShell
/// backend builds shell-level directory changes.
#[cfg(target_os = "windows")]
fn simplified_path(path: &Path) -> PathBuf {
    dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Command line for a **sandboxed** model command.
///
/// The working directory is also set from inside the shell: a native sandbox
/// (PSEC in particular) creates the process before the filesystem policy is
/// attached, so the kernel cannot open the requested directory and silently
/// falls back to the executable's own directory. `Set-Location` / `cd /d` makes
/// relative paths resolve in the requested directory on every tier.
fn sandbox_shell_command(
    workdir: &Path,
    command: &str,
    shell: &ModelShell,
) -> sandbox::SandboxCommand {
    sandbox::SandboxCommand::new(
        shell.program.clone().into_os_string(),
        shell.command_args(workdir, command),
    )
}

/// Command line for an unsandboxed hook command.
fn hook_shell_command(command: &str, shell: HookShell) -> Result<sandbox::SandboxCommand, String> {
    match shell {
        HookShell::Auto => Ok(auto_hook_shell_command(command)),
        HookShell::GitBash => {
            let program = hook_shell_program(shell)?;
            Ok(sandbox::SandboxCommand::new(
                program.into_os_string(),
                vec!["-c".into(), rewrite_nul_redirects(command).into()],
            ))
        }
        HookShell::Cmd => {
            let program = hook_shell_program(shell)?;
            Ok(sandbox::SandboxCommand::new(
                program.into_os_string(),
                vec!["/C".into(), command.into()],
            ))
        }
        HookShell::PowerShell | HookShell::Pwsh => {
            let program = hook_shell_program(shell)?;
            Ok(sandbox::SandboxCommand::new(
                program.into_os_string(),
                vec![
                    "-NoProfile".into(),
                    "-NonInteractive".into(),
                    "-Command".into(),
                    command.into(),
                ],
            ))
        }
        HookShell::Bash | HookShell::Sh | HookShell::Zsh => {
            let program = hook_shell_program(shell)?;
            Ok(sandbox::SandboxCommand::new(
                program.into_os_string(),
                vec!["-c".into(), command.into()],
            ))
        }
    }
}

fn auto_hook_shell_command(command: &str) -> sandbox::SandboxCommand {
    let command = command.to_string();
    if cfg!(target_os = "windows") {
        match windows_shell() {
            WindowsShell::GitBash(bash) => sandbox::SandboxCommand::new(
                bash.into_os_string(),
                vec!["-c".into(), rewrite_nul_redirects(&command).into()],
            ),
            WindowsShell::Cmd => {
                sandbox::SandboxCommand::new("cmd".into(), vec!["/C".into(), command.into()])
            }
        }
    } else {
        sandbox::SandboxCommand::new(unix_shell().into(), vec!["-c".into(), command.into()])
    }
}

fn prepare_process(cwd: &Path, command: &sandbox::SandboxCommand) -> tokio::process::Command {
    let mut process = tokio::process::Command::new(&command.program);
    process.args(&command.args);
    // Backend-produced overrides (the Windows launcher transport) are trusted
    // parent state, applied before the shared runtime environment.
    for (key, value) in &command.env {
        process.env(key, value);
    }
    apply_runtime_env(&mut process, cwd);
    process
}

/// Direct program spawn (hook kind=process): program + argv without any shell
/// interpretation. Same working directory / env hygiene / kill semantics as
/// prepare_shell. Note: on Windows this resolves programs like the OS does —
/// .cmd/.bat shims (npx & co.) are not runnable this way, use the shell kind.
pub(crate) fn prepare_program(
    cwd: &Path,
    program: &str,
    args: &[String],
) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(program);
    command.args(args);
    apply_runtime_env(&mut command, cwd);
    command
}

/// Shared tail of prepare_shell/prepare_program: working directory, env
/// hygiene, kill_on_drop and (on unix) an own process group for tree kills
fn apply_runtime_env(command: &mut tokio::process::Command, cwd: &Path) {
    command
        .no_console()
        .current_dir(cwd)
        .env("NO_COLOR", "1")
        .env("TERM", "dumb")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("PYTHONIOENCODING", "utf-8")
        .env("PYTHONUTF8", "1");
    if std::env::var_os("LANG").is_none() && std::env::var_os("LC_ALL").is_none() {
        command.env("LANG", "C.UTF-8");
    }
    command.kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
}

/// Model shell spawn: stdin null, stdout/stderr piped, under an explicit
/// filesystem profile. `ReadOnly` is for plan mode; `Unrestricted` (Full mode)
/// skips the OS sandbox entirely and therefore cannot honor a network block.
///
/// `workspace` is the policy root (workspace-write roots, deny-rule resolution)
/// and stays fixed; `workdir` is only the process working directory, so a
/// per-call `workdir` can never shrink or move the sandbox boundary.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_shell_with_profile(
    state: &SessionToolState,
    workspace: &Path,
    workdir: &Path,
    command: &str,
    shell: &ModelShell,
    sandbox_override: sandbox::SandboxOverride,
    profile: crate::task::SandboxProfile,
    additional: &crate::task::AdditionalPermissions,
) -> std::io::Result<super::process::ManagedChild> {
    if profile == crate::task::SandboxProfile::Unrestricted && sandbox::network_blocked_for(state) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Full mode runs without a sandbox and cannot enforce the configured network block; \
             switch to Auto or ReadOnly, or set `sandbox_network = \"allow\"`",
        ));
    }
    let command = sandbox::prepare_command(
        state,
        workspace,
        Some(workdir),
        sandbox_shell_command(workdir, command, shell),
        sandbox_override,
        profile,
        additional,
    )?;
    let mut process = prepare_process(workdir, &command);
    let _ = state;
    process
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    super::process::spawn(&mut process)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_cover_every_shell_shape() {
        let powershell = ModelShell {
            kind: ShellKind::PowerShell,
            program: PathBuf::from("pwsh.exe"),
            login: false,
        };
        assert_eq!(
            powershell.label(Some("7.4.6")),
            "PowerShell 7.4.6 (powershell -Command, PowerShell syntax)"
        );
        assert_eq!(
            powershell.label(None),
            "PowerShell (powershell -Command, PowerShell syntax)"
        );
        let bash = ModelShell {
            kind: ShellKind::Bash,
            program: PathBuf::from("/bin/bash"),
            login: false,
        };
        assert_eq!(bash.label(Some("5.2.21")), "bash 5.2.21 -c (Unix syntax)");
        let login_bash = ModelShell {
            login: true,
            ..bash
        };
        assert_eq!(
            login_bash.label(None),
            "bash -lc (Unix syntax, login shell)"
        );
        let cmd = ModelShell {
            kind: ShellKind::Cmd,
            program: PathBuf::from("cmd.exe"),
            login: false,
        };
        assert_eq!(
            cmd.label(Some("10.0.22631.4317")),
            "cmd 10.0.22631.4317 (cmd /C, Windows syntax)"
        );
    }

    fn hook_args(command: &sandbox::SandboxCommand) -> Vec<String> {
        command
            .args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn available_hook_shells_starts_with_auto_and_is_platform_gated() {
        let shells = available_hook_shells();
        assert_eq!(shells.first(), Some(&HookShell::Auto));
        for shell in &shells[1..] {
            assert!(platform_hook_shells().contains(shell), "{shell:?}");
            assert!(
                hook_shell_program(*shell).is_ok(),
                "{shell:?} is listed but does not resolve"
            );
        }
        #[cfg(windows)]
        {
            assert!(shells.contains(&HookShell::Cmd), "cmd is always present");
            assert!(
                !shells.contains(&HookShell::Bash),
                "Unix bash is not a Windows hook shell"
            );
            assert!(hook_shell_program(HookShell::Bash).is_err());
        }
        #[cfg(unix)]
        {
            assert!(!shells.contains(&HookShell::Cmd));
            assert!(hook_shell_program(HookShell::Cmd).is_err());
        }
    }

    #[test]
    fn explicit_hook_shell_command_lines_are_stable() {
        #[cfg(windows)]
        {
            let cmd = hook_shell_command("echo hi", HookShell::Cmd).expect("cmd");
            assert_eq!(hook_args(&cmd), vec!["/C", "echo hi"]);
            if hook_shell_program(HookShell::PowerShell).is_ok() {
                let powershell =
                    hook_shell_command("echo hi", HookShell::PowerShell).expect("powershell");
                assert_eq!(
                    hook_args(&powershell),
                    vec!["-NoProfile", "-NonInteractive", "-Command", "echo hi"]
                );
            }
            if detect_git_bash().is_some() {
                let git_bash =
                    hook_shell_command("echo hi >nul", HookShell::GitBash).expect("git bash");
                assert_eq!(hook_args(&git_bash), vec!["-c", "echo hi >/dev/null"]);
            }
        }
        #[cfg(unix)]
        {
            for shell in [HookShell::Bash, HookShell::Sh, HookShell::Zsh] {
                if hook_shell_program(shell).is_ok() {
                    let command = hook_shell_command("echo hi", shell).expect("unix shell");
                    assert_eq!(hook_args(&command), vec!["-c", "echo hi"], "{shell:?}");
                }
            }
        }
    }

    #[test]
    fn wrong_platform_shell_names_itself_in_the_error() {
        #[cfg(windows)]
        let error = hook_shell_program(HookShell::Zsh).unwrap_err();
        #[cfg(unix)]
        let error = hook_shell_program(HookShell::Cmd).unwrap_err();
        assert!(error.contains("not available on this platform"), "{error}");
    }
}
