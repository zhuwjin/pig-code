use super::*;

/// Dangerous-command reason: key feeds the GUI approval dialog (core.danger.* localized via t!),
/// en feeds the model-facing rejection receipt (both channels share the same decision).
#[derive(Clone, Copy, Debug)]
pub struct DangerReason {
    pub key: &'static str,
    pub en: &'static str,
}

/// Conservative destructive-command blocklist (no AST; only explicit shapes are intercepted; a hit is rejected with the reason stated).
/// Prefer false positives over letting things through; the false-positive message guides the user to run it manually. Returning Some(reason) means the command should be blocked.
pub fn is_dangerous_command(command: &str) -> Option<DangerReason> {
    let tokens: Vec<&str> = command.split_whitespace().collect();
    let compact: String = command.chars().filter(|c| !c.is_whitespace()).collect();

    // fork bomb: :(){ :|:& };:
    if compact.contains(":(){") && compact.contains("|:&") {
        return Some(DangerReason {
            key: "fork_bomb",
            en: "fork bomb",
        });
    }

    // Command-position check: the first token, or one following ; & | && || sudo then do if ! (
    let in_command_position = |i: usize| {
        if i == 0 {
            return true;
        }
        let prev = tokens[i - 1];
        prev.ends_with(';')
            || prev.ends_with('&')
            || matches!(
                prev,
                "|" | "&&" | "||" | ";" | "(" | "sudo" | "then" | "do" | "if" | "!"
            )
    };
    fn base_name(token: &str) -> &str {
        token.rsplit('/').next().unwrap_or(token)
    }

    for (i, token) in tokens.iter().enumerate() {
        let base = base_name(token);
        if !in_command_position(i) {
            continue;
        }
        // rm -rf/-fr with a target of / /* ~ ~/ $HOME . (an ordinary rm -rf node_modules is allowed)
        if base == "rm" {
            let mut recursive_force = false;
            let mut dangerous_target = false;
            for t in &tokens[i + 1..] {
                if t.starts_with('-') && !t.starts_with("--") {
                    let flags = t.trim_start_matches('-');
                    if (flags.contains('r') || flags.contains('R')) && flags.contains('f') {
                        recursive_force = true;
                    }
                } else if matches!(*t, "/" | "/*" | "~" | "~/" | "$HOME" | ".") {
                    dangerous_target = true;
                }
            }
            if recursive_force && dangerous_target {
                return Some(DangerReason {
                    key: "rm_rf_root",
                    en: "rm -rf targeting the root, home, or current directory",
                });
            }
        }
        // Disk formatting/partitioning
        if base == "mkfs" || base.starts_with("mkfs.") || base == "fdisk" {
            return Some(DangerReason {
                key: "disk_format",
                en: "disk formatting/partitioning",
            });
        }
        if base == "diskutil"
            && tokens
                .get(i + 1)
                .is_some_and(|next| next.starts_with("erase"))
        {
            return Some(DangerReason {
                key: "disk_format",
                en: "disk formatting/partitioning",
            });
        }
        // dd writing to a block device (of=/dev/…; character devices are allowlisted)
        if base == "dd" {
            for t in &tokens[i + 1..] {
                if let Some(target) = t.strip_prefix("of=")
                    && target.starts_with("/dev/")
                    && !matches!(
                        target,
                        "/dev/null" | "/dev/zero" | "/dev/random" | "/dev/urandom"
                    )
                {
                    return Some(DangerReason {
                        key: "dd_block",
                        en: "dd writing to a block device",
                    });
                }
            }
        }
        // Shutdown/reboot
        if matches!(base, "shutdown" | "reboot" | "halt" | "poweroff") {
            return Some(DangerReason {
                key: "shutdown",
                en: "shutdown/reboot",
            });
        }
        if base == "systemctl"
            && tokens
                .get(i + 1)
                .is_some_and(|next| matches!(*next, "poweroff" | "reboot" | "halt" | "kexec"))
        {
            return Some(DangerReason {
                key: "shutdown",
                en: "shutdown/reboot",
            });
        }
        if base == "init"
            && tokens
                .get(i + 1)
                .is_some_and(|next| matches!(*next, "0" | "6"))
        {
            return Some(DangerReason {
                key: "shutdown",
                en: "shutdown/reboot",
            });
        }
        // Recursive permission/ownership change on the root directory
        if base == "chmod" || base == "chown" {
            let recursive = tokens[i + 1..]
                .iter()
                .any(|t| t.starts_with('-') && t.trim_start_matches('-').contains('R'));
            let root_target = tokens[i + 1..].contains(&"/");
            let is_777 = tokens[i + 1..].contains(&"777");
            if recursive && root_target && (base == "chown" || is_777) {
                return Some(DangerReason {
                    key: "chmod_root",
                    en: "recursive chmod/chown on /",
                });
            }
        }
        // git push --force / -f is not blocked (a common operation; approval mode is the safety net)
    }
    None
}

impl Tool for Bash {
    fn name(&self) -> &'static str {
        "Bash"
    }

    fn is_shell(&self) -> bool {
        true
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "Bash",
                "description": "Execute a shell command and return stdout/stderr with the exit code. The working directory is the workspace root. High-risk commands trigger a user confirmation dialog. Shell selection: Unix prefers native bash (falling back to sh); Windows prefers Git Bash (Unix syntax), falling back to cmd /C when Git Bash is not installed — see the Shell line of the env block in the system prompt. NO_COLOR=1 / TERM=dumb / GIT_TERMINAL_PROMPT=0 are injected (git never blocks on an interactive prompt). timeout defaults to 60s and is capped at 300s; on timeout the command is moved to a background task and keeps running (no output lost). Output over 30KB is spilled to .pigcode/tool-results/ with a head/tail preview returned; a cumulative 16MiB hard stop applies. Long-lived commands (dev servers, watchers, long builds) can also be run directly in the background with run_in_background.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "The command to execute" },
                        "run_in_background": { "type": "boolean", "description": "true runs the command in the background and returns a task_id immediately (default false)" },
                        "timeout": { "type": "integer", "description": "Timeout in seconds; default 60, max 300. On timeout the command moves to a background task and keeps running without losing output" }
                    },
                    "required": ["command"]
                }
            }
        })
    }

    fn execute<'a>(
        &'a self,
        args: serde_json::Value,
        ctx: ToolContext<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<ToolEffect, String>> + Send + 'a>> {
        Box::pin(async move {
            let command = args["command"]
                .as_str()
                .ok_or("Missing required parameter: command")?;
            // The blocklist (is_dangerous_command) is enforced at the session layer: a hit → forced approval dialog;
            // execution reaches here only after the user allows; the execute layer no longer hard-rejects.
            if args["run_in_background"].as_bool().unwrap_or(false) {
                let task_id = crate::task::spawn_background(ctx.state, ctx.cwd, command);
                return Ok(ToolEffect::plain(format!(
                    "Started in the background, task_id: {task_id}. Use TaskOutput to check its output and TaskStop to stop it."
                )));
            }
            let secs = args["timeout"].as_u64().unwrap_or(60).clamp(1, 300);
            match crate::task::run_foreground(
                ctx.state,
                ctx.cwd,
                command,
                std::time::Duration::from_secs(secs),
            )
            .await
            {
                crate::task::ForegroundOutcome::SpawnFailed { error } => {
                    Err(format!("Failed to start command: {error}"))
                }
                crate::task::ForegroundOutcome::TimedOut { task_id } => {
                    Ok(ToolEffect::plain(format!(
                        "Command did not finish within {secs}s; moved to background task {task_id} (output is preserved). Use TaskOutput to check it and TaskStop to stop it."
                    )))
                }
                crate::task::ForegroundOutcome::Completed {
                    output,
                    code,
                    spill_path,
                } => {
                    let mut text = output;
                    if text.chars().count() <= MAX_BASH_OUTPUT {
                        // Small outputs leave no trace: remove the spill file
                        if let Some(path) = &spill_path {
                            let _ = std::fs::remove_file(path);
                        }
                        text.push_str(&format!("\n[exit code: {code}]"));
                    } else {
                        // Head/tail preview + the full output in the spill file (the registry output has a 64KB rolling cap and does not count)
                        let total = text.chars().count();
                        let head: String = text.chars().take(4096).collect();
                        let tail = crate::task::tail_chars(&text, 1024);
                        let spill_display = spill_path
                            .as_ref()
                            .map(|path| {
                                path.strip_prefix(ctx.cwd)
                                    .map(|relative| relative.to_string_lossy().replace('\\', "/"))
                                    .unwrap_or_else(|_| path.display().to_string())
                            })
                            .unwrap_or_else(|| "(unknown path)".to_string());
                        text = format!(
                            "{head}\n\n[...middle omitted...]\n\n{tail}\n\n[Output too long ({total} chars total); the full output was saved to {spill_display} — page through it with Read]\n[exit code: {code}]"
                        );
                    }
                    Ok(ToolEffect::plain(text))
                }
            }
        })
    }
}
