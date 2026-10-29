//! Windows shell backends: the PowerShell chain (pwsh → powershell → cmd) and
//! cmd. Split from the shared shell module so bash/zsh live in their own file
//! (same shape as Codex's `shell-command` crate: `bash.rs` + `powershell.rs`).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::NoConsoleExt as _;

use super::{
    ModelShell, ShellKind, WindowsSandboxShell, rewrite_nul_redirects, simplified_path,
    windows_sandbox_shell,
};

/// Names a Windows model command may request. Git Bash/MSYS2 is deliberately
/// absent: it cannot initialize under the PSEC AppContainer token
/// (microsoft/mxc#1061, openai/codex#54449), so accepting `bash` here would
/// only produce a command that fails inside the sandbox.
const ALLOWED: &[&str] = &["powershell", "pwsh", "cmd"];

/// Codex's prefix: force UTF-8 console output so the decoder sees one encoding.
const POWERSHELL_UTF8_PREFIX: &str =
    "try { [Console]::OutputEncoding=[System.Text.Encoding]::UTF8 } catch {}\n";

fn normalize(request: &str) -> String {
    let path = Path::new(request.trim());
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or(request.trim())
        .to_ascii_lowercase()
}

fn cmd_program() -> PathBuf {
    std::env::var_os("SystemRoot")
        .map(|root| PathBuf::from(root).join("System32").join("cmd.exe"))
        .filter(|candidate| candidate.is_file())
        .unwrap_or_else(|| PathBuf::from("cmd.exe"))
}

/// Default Windows shell: the same chain the sandbox uses (PowerShell with a
/// cmd fallback). `login` is meaningless here: PowerShell always runs with
/// `-NoProfile` so the sandboxed environment is deterministic.
pub(crate) fn default_shell() -> ModelShell {
    match windows_sandbox_shell() {
        WindowsSandboxShell::PowerShell(program) => ModelShell {
            kind: ShellKind::PowerShell,
            program,
            login: false,
        },
        WindowsSandboxShell::Cmd => ModelShell {
            kind: ShellKind::Cmd,
            program: cmd_program(),
            login: false,
        },
    }
}

pub(crate) fn resolve(request: Option<&str>, login: bool) -> Result<ModelShell, String> {
    let Some(request) = request.map(str::trim).filter(|name| !name.is_empty()) else {
        // The default ignores `login`: Windows shells do not load profiles in
        // the sandbox, so accepting the flag silently would be a lie.
        let _ = login;
        return Ok(default_shell());
    };
    match normalize(request).as_str() {
        "powershell" | "pwsh" => match windows_sandbox_shell() {
            WindowsSandboxShell::PowerShell(program) => Ok(ModelShell {
                kind: ShellKind::PowerShell,
                program,
                login: false,
            }),
            WindowsSandboxShell::Cmd => Err(
                "PowerShell is not installed on this machine (or only a Store-packaged \
                 build exists, which cannot run inside the sandbox); choose `cmd`"
                    .to_string(),
            ),
        },
        "cmd" => Ok(ModelShell {
            kind: ShellKind::Cmd,
            program: cmd_program(),
            login: false,
        }),
        _ => Err(format!(
            "`{request}` is not an available shell on Windows; choose one of {}. Git Bash \
             cannot run inside the Windows sandbox (MSYS2 cannot initialize under the PSEC \
             AppContainer token)",
            ALLOWED.join(", ")
        )),
    }
}

/// Command line for a Windows model command. The working directory is set from
/// inside the shell as well: a native sandbox (PSEC in particular) creates the
/// process before the filesystem policy is attached, so the kernel cannot open
/// the requested directory and silently falls back to the executable's own
/// directory.
pub(crate) fn command_args(shell: &ModelShell, workdir: &Path, command: &str) -> Vec<OsString> {
    let directory = simplified_path(workdir).display().to_string();
    match shell.kind {
        ShellKind::PowerShell => {
            let quoted = directory.replace('\'', "''");
            vec![
                OsString::from("-NoLogo"),
                OsString::from("-NoProfile"),
                OsString::from("-Command"),
                OsString::from(format!(
                    "Set-Location -LiteralPath '{quoted}' -ErrorAction Stop\n\
                     {POWERSHELL_UTF8_PREFIX}{command}\n\
                     if (-not $?) {{ exit 1 }}\n"
                )),
            ]
        }
        ShellKind::Cmd => vec![
            OsString::from("/C"),
            OsString::from(format!(
                "cd /d \"{directory}\" && {}",
                rewrite_nul_redirects(command)
            )),
        ],
        _ => unreachable!("non-Windows shell kind reached the Windows backend"),
    }
}

/// Probe the shell version once per process. PowerShell is started with
/// `-NoProfile -NonInteractive` so a profile cannot print or block.
pub(crate) fn version(shell: &ModelShell) -> Option<String> {
    match shell.kind {
        ShellKind::PowerShell => run_version(
            &shell.program,
            &[
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "$PSVersionTable.PSVersion.ToString()",
            ],
            parse_powershell_version,
        ),
        ShellKind::Cmd => run_version(&shell.program, &["/C", "ver"], parse_cmd_version),
        _ => None,
    }
}

fn run_version(program: &Path, args: &[&str], parse: fn(&str) -> Option<String>) -> Option<String> {
    let output = std::process::Command::new(program)
        .no_console()
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    parse(&String::from_utf8_lossy(&output.stdout))
}

fn parse_powershell_version(text: &str) -> Option<String> {
    let version = text.lines().next()?.trim();
    let valid = version
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_digit());
    valid.then(|| version.to_string())
}

fn parse_cmd_version(text: &str) -> Option<String> {
    let rest = text.split("[Version ").nth(1)?;
    let version = rest.split(']').next()?.trim();
    (!version.is_empty()).then(|| version.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_versions() {
        assert_eq!(
            parse_powershell_version("7.4.6\r\n"),
            Some("7.4.6".to_string())
        );
        assert_eq!(
            parse_cmd_version("Microsoft Windows [Version 10.0.22631.4317]\r\n"),
            Some("10.0.22631.4317".to_string())
        );
        assert_eq!(parse_powershell_version("no version here"), None);
    }

    #[test]
    fn rejects_non_windows_shells_with_guidance() {
        let error = resolve(Some("bash"), false).unwrap_err();
        assert!(
            error.contains("not an available shell on Windows"),
            "{error}"
        );
        assert!(error.contains("Git Bash"), "{error}");
        assert!(error.contains("powershell"), "{error}");
    }
}
