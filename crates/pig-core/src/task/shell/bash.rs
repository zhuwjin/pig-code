//! Unix shell backends: bash / zsh / sh detection, version probing, and the
//! `-c` / `-lc` command line. Split from the shared shell module so the
//! PowerShell path lives next to its own detection code (same shape as
//! Codex's `shell-command` crate: `bash.rs` + `powershell.rs`).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::{ModelShell, ShellKind};

/// Names a Unix model command may request. Anything else is rejected with the
/// list instead of silently falling back (the model should know what ran).
const ALLOWED: &[&str] = &["bash", "zsh", "sh"];

fn normalize(request: &str) -> String {
    let path = Path::new(request.trim());
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or(request.trim())
        .to_ascii_lowercase()
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

pub(crate) fn program_for(kind: ShellKind) -> Option<PathBuf> {
    let (name, fallbacks): (&str, &[&str]) = match kind {
        ShellKind::Bash => (
            "bash",
            &["/bin/bash", "/usr/bin/bash", "/usr/local/bin/bash"],
        ),
        ShellKind::Zsh => ("zsh", &["/bin/zsh", "/usr/bin/zsh", "/usr/local/bin/zsh"]),
        ShellKind::Sh => ("sh", &["/bin/sh", "/usr/bin/sh"]),
        // Windows kinds never reach this module.
        ShellKind::PowerShell | ShellKind::Cmd => return None,
    };
    find_in_path(name).or_else(|| {
        fallbacks
            .iter()
            .map(PathBuf::from)
            .find(|candidate| candidate.is_file())
    })
}

/// Default Unix shell: native bash (bashisms like `[[ ]]`/arrays fail under
/// dash-style sh), falling back to sh when no bash is installed.
pub(crate) fn default_shell() -> ModelShell {
    let kind = if program_for(ShellKind::Bash).is_some() {
        ShellKind::Bash
    } else {
        ShellKind::Sh
    };
    ModelShell {
        kind,
        program: program_for(kind).unwrap_or_else(|| PathBuf::from("/bin/sh")),
        login: false,
    }
}

/// Resolve an optional model-requested shell name. `None` keeps the session
/// default; an explicit name must be one of [`ALLOWED`] and installed.
pub(crate) fn resolve(request: Option<&str>, login: bool) -> Result<ModelShell, String> {
    let Some(request) = request.map(str::trim).filter(|name| !name.is_empty()) else {
        return Ok(ModelShell {
            login,
            ..default_shell()
        });
    };
    let name = normalize(request);
    let kind = match name.as_str() {
        "bash" => ShellKind::Bash,
        "zsh" => ShellKind::Zsh,
        "sh" => ShellKind::Sh,
        _ => {
            return Err(format!(
                "`{request}` is not an available shell on this platform; choose one of {}",
                ALLOWED.join(", ")
            ));
        }
    };
    let program = program_for(kind).ok_or_else(|| {
        format!(
            "the requested shell `{name}` is not installed; available shells: {}",
            available().join(", ")
        )
    })?;
    Ok(ModelShell {
        kind,
        program,
        login,
    })
}

fn available() -> Vec<&'static str> {
    ALLOWED
        .iter()
        .copied()
        .filter(|name| {
            let kind = match *name {
                "bash" => ShellKind::Bash,
                "zsh" => ShellKind::Zsh,
                _ => ShellKind::Sh,
            };
            program_for(kind).is_some()
        })
        .collect()
}

/// `-lc` loads the user's profile (PATH entries added by nvm/pyenv/…), `-c`
/// runs a plain non-login shell. `login` is the model's per-call opt-in.
pub(crate) fn command_args(shell: &ModelShell, command: &str) -> Vec<OsString> {
    let flag = if shell.login { "-lc" } else { "-c" };
    vec![OsString::from(flag), OsString::from(command)]
}

/// Probe the shell version once per process (no profile loading; `--version`
/// prints and exits). Parsers are pure and unit-tested.
pub(crate) fn version(shell: &ModelShell) -> Option<String> {
    if matches!(shell.kind, ShellKind::Sh) {
        return None;
    }
    let output = std::process::Command::new(&shell.program)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    match shell.kind {
        ShellKind::Bash => parse_bash_version(&text),
        ShellKind::Zsh => parse_zsh_version(&text),
        _ => None,
    }
}

fn parse_bash_version(text: &str) -> Option<String> {
    let rest = text.lines().next()?.split("version ").nth(1)?;
    let version: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    (!version.is_empty()).then_some(version)
}

fn parse_zsh_version(text: &str) -> Option<String> {
    let version = text.lines().next()?.split_whitespace().nth(1)?;
    let version = version.trim_start_matches('v');
    (!version.is_empty()).then(|| version.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_versions() {
        assert_eq!(
            parse_bash_version("GNU bash, version 5.2.21(1)-release (x86_64-apple-darwin23)"),
            Some("5.2.21".to_string())
        );
        assert_eq!(
            parse_zsh_version("zsh 5.9 (x86_64-apple-darwin23.0)"),
            Some("5.9".to_string())
        );
        assert_eq!(parse_bash_version("not a bash banner"), None);
    }

    #[test]
    fn normalizes_paths_and_case() {
        assert_eq!(normalize("/bin/bash"), "bash");
        assert_eq!(normalize("ZSH"), "zsh");
        assert_eq!(normalize("/usr/local/bin/sh"), "sh");
    }

    #[test]
    fn rejects_shells_outside_the_unix_allowlist() {
        let error = resolve(Some("powershell"), false).unwrap_err();
        assert!(error.contains("not an available shell"), "{error}");
        assert!(error.contains("bash"), "{error}");
    }
}
