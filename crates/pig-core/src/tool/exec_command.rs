use super::*;

/// Windows-only guidance appended to the tool description (same three rules
/// Codex adds for its unified exec tool). They are prompt guidance, not a
/// sandbox: the command classifier still runs independently at the session
/// layer.
const WINDOWS_SAFETY_GUIDANCE: &str = "\n\nWindows safety rules:\n\
- Do not compose destructive filesystem commands across shells. Do not enumerate paths in \
PowerShell and then pass them to `cmd /c`, batch builtins, or another shell for deletion or \
moving. Use one shell end-to-end, prefer native PowerShell cmdlets such as `Remove-Item` / \
`Move-Item` with `-LiteralPath`, and avoid string-built shell commands for file operations.\n\
- Before any recursive delete or move on Windows, verify the resolved absolute target paths \
stay within the intended workspace or explicitly named target directory. Never issue a \
recursive delete or move against a computed path if the final target has not been checked.\n\
- When using `Start-Process` to launch a background helper or service, pass `-WindowStyle \
Hidden` unless the user explicitly asked for a visible interactive window. Use visible \
windows only for interactive tools the user needs to see or control.";

/// Canonicalize through `dunce` on Windows (it strips the `\\?\` verbatim
/// prefix `std::fs` leaves behind) and `std::fs` elsewhere — `dunce` is a
/// Windows-only dependency.
fn canonicalize(path: &Path) -> std::io::Result<PathBuf> {
    #[cfg(windows)]
    {
        dunce::canonicalize(path)
    }
    #[cfg(not(windows))]
    {
        std::fs::canonicalize(path)
    }
}

fn shell_choices() -> (&'static [&'static str], &'static str) {
    #[cfg(windows)]
    {
        (
            &["powershell", "pwsh", "cmd"],
            "PowerShell (pwsh preferred) or cmd; Git Bash cannot run inside the Windows sandbox",
        )
    }
    #[cfg(not(windows))]
    {
        (
            &["bash", "zsh", "sh"],
            "bash (default), zsh or sh; an explicit shell must be installed",
        )
    }
}

/// Resolve the per-call `workdir`: relative paths are based on the workspace
/// root, and the directory must exist. The sandbox boundary is unaffected — the
/// policy root stays the session workspace, so this can only change where the
/// process starts, never what it may write.
///
/// The filesystem calls run on the blocking pool: a model-supplied path can be
/// a dead SMB share, and `canonicalize` may sit in an OS timeout for seconds;
/// that must not stall the core's async runtime.
async fn resolve_workdir(
    workspace: &Path,
    raw: Option<&str>,
) -> Result<std::path::PathBuf, String> {
    let Some(raw) = raw.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(workspace.to_path_buf());
    };
    let candidate = Path::new(raw);
    let joined = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        workspace.join(candidate)
    };
    tokio::task::spawn_blocking(move || {
        let canonical = canonicalize(&joined)
            .map_err(|error| format!("workdir `{}` cannot be opened: {error}", joined.display()))?;
        if !canonical.is_dir() {
            return Err(format!(
                "workdir `{}` is not a directory",
                canonical.display()
            ));
        }
        Ok(canonical)
    })
    .await
    .map_err(|error| format!("workdir resolution failed: {error}"))?
}

impl Tool for ExecCommand {
    fn name(&self) -> &'static str {
        EXEC_COMMAND_TOOL
    }

    fn is_shell(&self) -> bool {
        true
    }

    fn prefer_strict(&self) -> bool {
        true
    }

    fn schema(&self) -> serde_json::Value {
        let (choices, shell_hint) = shell_choices();
        let mut description = format!(
            "Execute a shell command and return stdout/stderr with the exit code. This one tool \
serves every shell: `shell` picks the interpreter for this call (allowed here: {}; {}), and \
omitting it uses the session shell shown on the Shell line of the env block. `workdir` chooses \
the working directory (relative paths are based on the workspace root; default: workspace \
root). On Unix, `login: true` runs the shell as a login shell (`-lc`) so the user's profile is \
loaded; the default is false (non-login `-c`, faster and deterministic). Windows shells always \
run with -NoProfile, so `login` is ignored there. In ReadOnly mode the command runs in a \
read-only sandbox: read-only commands need no approval, while a command that needs to write \
must request `sandbox_permissions: \"with_additional_permissions\"` and list the directories \
in `additional_permissions.file_system.write`; the user is asked once and only those \
directories become writable for that one command. In Auto the command runs with \
workspace-write; in Full it runs without a sandbox (dangerous commands are rejected). On \
macOS, the active filesystem read/write roots are enforced with Seatbelt for the command and \
its children. High-risk commands ask for confirmation in ReadOnly/Auto and are rejected in \
Full. NO_COLOR=1 / TERM=dumb / GIT_TERMINAL_PROMPT=0 are injected (git never blocks on an \
interactive prompt). timeout defaults to 60s and is capped at 300s; on timeout the command is \
moved to a background task and keeps running (no output lost). Output over 30KB is spilled to \
the session's tool-results log with a head/tail preview returned; a cumulative 16MiB hard stop \
applies. Long-lived commands (dev servers, watchers, long builds) can also be run directly in \
the background with run_in_background.",
            choices.join(", "),
            shell_hint
        );
        if cfg!(target_os = "windows") {
            description.push_str(WINDOWS_SAFETY_GUIDANCE);
        }
        serde_json::json!({
            "type": "function",
            "function": {
                "name": EXEC_COMMAND_TOOL,
                "description": description,
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "The command to execute" },
                        "shell": {
                            "type": "string",
                            "enum": choices,
                            "description": format!("Shell to run this command with; omit to use the session shell from the Shell line. {shell_hint}.")
                        },
                        "workdir": { "type": "string", "description": "Working directory for this command; relative paths are based on the workspace root. Defaults to the workspace root. Must be an existing directory." },
                        "login": { "type": "boolean", "description": "Unix only: true runs a login shell (-lc), loading the user's profile (PATH additions from nvm/pyenv/...). Default false (-c). Ignored on Windows, where PowerShell always runs with -NoProfile." },
                        "run_in_background": { "type": "boolean", "description": "true runs the command in the background and returns a task_id immediately (default false)" },
                        "timeout": { "type": "integer", "description": "Timeout in seconds; default 60, max 300. On timeout the command moves to a background task and keeps running without losing output" },
                        "sandbox_permissions": {
                            "type": "string",
                            "enum": ["use_default", "with_additional_permissions"],
                            "description": "Defaults to `use_default`. `with_additional_permissions` adds the absolute directories or single files in `additional_permissions.file_system.write` for this one command while the sandbox stays on. `require_escalated` asks to run this one command outside the sandbox (approval is mandatory; with explicit read-deny rules the sandbox stays on with a full-disk write and the denies preserved). Read access is already full-disk, so it needs no request, except a file inside a profile credential directory (~/.ssh, ~/.config, ...), which needs a request naming that exact single file under `additional_permissions.file_system.read` (a directory there is never approved). Full already runs without a sandbox."
                        },
                        "additional_permissions": {
                            "type": "object",
                            "description": "Extra permissions for this command; only with `sandbox_permissions: \"with_additional_permissions\"`.",
                            "properties": {
                                "file_system": {
                                    "type": "object",
                                    "properties": {
                                        "read": {
                                            "type": "array",
                                            "items": { "type": "string" },
                                            "description": "Absolute paths of single files, or one directory to list, inside a profile credential directory. Listing reveals entry names only (reading them still fails); reading elsewhere is already allowed."
                                        },
                                        "write": {
                                            "type": "array",
                                            "items": { "type": "string" },
                                            "description": "Absolute paths of existing directories or files to make writable for this command. A file path grants exactly that file, which is the only shape the user can approve inside a protected credential directory."
                                        }
                                    }
                                }
                            }
                        },
                        "justification": {
                            "type": "string",
                            "description": "One sentence shown to the user explaining why this command needs the extra access or the one unsandboxed run; omit otherwise."
                        }
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
            let shell =
                crate::task::resolve_model_shell(args["shell"].as_str(), args["login"].as_bool())
                    .map_err(|error| format!("Invalid shell selection: {error}"))?;
            let workdir = resolve_workdir(ctx.cwd, args["workdir"].as_str()).await?;
            // The classifier is enforced at the session layer: a hit either asks
            // for approval or is rejected outright, depending on the mode.
            if args["run_in_background"].as_bool().unwrap_or(false) {
                let task_id = crate::task::spawn_background_with_profile(
                    ctx.state,
                    ctx.cwd,
                    &workdir,
                    command,
                    &shell,
                    // The approved `require_escalated` profile is a property of
                    // this command, not of foreground vs background. Dropping it
                    // here silently downgraded an approved background escalation
                    // to the ordinary workspace-write sandbox.
                    ctx.sandbox_override,
                    ctx.sandbox_profile,
                    &ctx.additional_permissions,
                );
                return Ok(ToolEffect::plain(format!(
                    "Started in the background, task_id: {task_id}. Use TaskOutput to check its output and TaskStop to stop it."
                )));
            }
            let secs = args["timeout"].as_u64().unwrap_or(60).clamp(1, 300);
            match crate::task::run_foreground_with_profile(
                ctx.state,
                ctx.cwd,
                &workdir,
                command,
                &shell,
                std::time::Duration::from_secs(secs),
                ctx.sandbox_override,
                ctx.sandbox_profile,
                &ctx.additional_permissions,
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
                    if text.chars().count() <= MAX_EXEC_OUTPUT {
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
                        // The spill lives under the session data root (already an
                        // always-readable root for the model), outside the
                        // workspace: report its absolute path.
                        let spill_display = spill_path
                            .as_ref()
                            .map(|path| path.display().to_string())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn workdir_defaults_to_the_workspace_and_requires_a_directory() {
        // Sessions hand the tool a canonical workspace; macOS's `temp_dir()`
        // sits behind the `/var` → `/private/var` symlink, so canonicalize the
        // fixture instead of assuming the raw path already matches.
        let workspace = canonicalize(&std::env::temp_dir()).unwrap();
        assert_eq!(resolve_workdir(&workspace, None).await.unwrap(), workspace);
        let nested = workspace.join("pig-workdir-test");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(
            resolve_workdir(&workspace, Some("pig-workdir-test"))
                .await
                .unwrap(),
            canonicalize(&nested).unwrap()
        );
        let error = resolve_workdir(&workspace, Some("pig-missing-workdir"))
            .await
            .unwrap_err();
        assert!(error.contains("cannot be opened"), "{error}");
        let _ = std::fs::remove_dir_all(&nested);
    }

    #[test]
    fn schema_names_the_canonical_tool_and_offers_shell_and_workdir() {
        let schema = ExecCommand.schema();
        assert_eq!(schema["function"]["name"], EXEC_COMMAND_TOOL);
        let properties = &schema["function"]["parameters"]["properties"];
        assert!(properties.get("shell").is_some());
        assert!(properties.get("workdir").is_some());
        assert!(properties.get("login").is_some());
        let choices: Vec<&str> = properties["shell"]["enum"]
            .as_array()
            .expect("shell enum")
            .iter()
            .filter_map(|value| value.as_str())
            .collect();
        let expected = if cfg!(windows) { "powershell" } else { "bash" };
        assert!(
            choices.contains(&expected),
            "shell allowlist for this platform should contain {expected}: {choices:?}"
        );
        assert!(
            !choices.contains(&if cfg!(windows) { "bash" } else { "powershell" }),
            "the other platform's shells are not on this allowlist: {choices:?}"
        );
        let description = schema["function"]["description"]
            .as_str()
            .expect("description");
        assert_eq!(
            description.contains("Windows safety rules"),
            cfg!(windows),
            "Windows safety guidance is appended exactly on Windows"
        );
    }
}
