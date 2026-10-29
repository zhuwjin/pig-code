//! Hook subprocess execution: kind=command runs the entry's command string
//! through the same shell resolution as the Bash tool (prepare_shell: Windows
//! Git Bash chain / Unix bash→sh), kind=process spawns the program + args
//! directly (prepare_program, no shell interpretation); both share the
//! no-console / env-hygiene / process-group setup, the event payload on stdin
//! and both output streams capped at max_output_bytes. Timeout and
//! cancellation kill the process tree.

use std::time::Duration;

use pig_protocol::{HookEntry, HookKind};
use tokio::io::AsyncReadExt as _;
use tokio::io::AsyncWriteExt as _;

use super::HookRunCtx;
use super::output::{HookRaw, RawOutcome};

/// Hard cap mirrors the Bash tool's max timeout
const MAX_TIMEOUT_MS: u64 = 600_000;

pub(crate) async fn run_entry(
    entry: &HookEntry,
    payload: &super::HookPayload,
    run: &HookRunCtx,
    default_timeout_ms: u64,
    max_output_bytes: usize,
) -> HookRaw {
    let timeout_ms = entry
        .timeout_ms
        .unwrap_or(default_timeout_ms)
        .clamp(1, MAX_TIMEOUT_MS);
    // Materialize the transcript snapshot for this run; the guard deletes it
    // on every exit path (timeout/cancel included)
    let transcript = TranscriptFile::write(&payload.transcript_lines);
    let payload_json = payload.to_json_line(transcript.path_str());
    // ${VAR} expansion (ZCode/Claude convention): the injected hook variables
    // plus the process environment; unknown names stay literal
    let command_text = expand_vars(&entry.command, &run.session_id, &run.cwd);
    let args: Vec<String> = entry
        .args
        .iter()
        .map(|arg| expand_vars(arg, &run.session_id, &run.cwd))
        .collect();
    let mut command = match entry.kind {
        HookKind::Command => {
            match crate::task::prepare_shell(&run.cwd, &command_text, entry.shell) {
                Ok(command) => command,
                Err(error) => {
                    return HookRaw {
                        outcome: RawOutcome::SpawnFailed(error),
                        stdout: String::new(),
                        stderr: String::new(),
                    };
                }
            }
        }
        HookKind::Process => crate::task::prepare_program(&run.cwd, &command_text, &args),
    };
    command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .env("PIG_SESSION_ID", &run.session_id)
        .env("PIG_PROJECT_DIR", run.cwd.display().to_string())
        // Claude-compatible aliases: existing hook scripts keep working
        .env("CLAUDE_SESSION_ID", &run.session_id)
        .env("CLAUDE_CODE_SESSION_ID", &run.session_id)
        .env("CLAUDE_PROJECT_DIR", run.cwd.display().to_string());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return HookRaw {
                outcome: RawOutcome::SpawnFailed(error.to_string()),
                stdout: String::new(),
                stderr: String::new(),
            };
        }
    };
    let pid = child.id();
    let mut stdin_handle = child.stdin.take();
    let mut stdout_handle = child.stdout.take();
    let mut stderr_handle = child.stderr.take();
    let mut stdout_buf: Vec<u8> = Vec::new();
    let mut stderr_buf: Vec<u8> = Vec::new();
    // take(max + 1) so a full-cap read is detectable (len > max = truncated)
    let read_cap = max_output_bytes as u64 + 1;
    let timed = tokio::time::timeout(Duration::from_millis(timeout_ms), async {
        // All four legs run concurrently: a hook reading stdin lazily while
        // writing stdout must not deadlock against the payload write, and the
        // wait must not be polled before the pipes (the child could be
        // blocked on a full pipe we are not draining yet)
        let write = async {
            if let Some(mut stdin) = stdin_handle.take() {
                let _ = stdin.write_all(payload_json.as_bytes()).await;
                // tokio's ChildStdin::shutdown() is a no-op — only dropping
                // the handle closes the pipe, and hooks reading stdin to EOF
                // would hang without it
                drop(stdin);
            }
        };
        let read_out = async {
            if let Some(pipe) = stdout_handle.as_mut() {
                let _ = pipe.take(read_cap).read_to_end(&mut stdout_buf).await;
            }
        };
        let read_err = async {
            if let Some(pipe) = stderr_handle.as_mut() {
                let _ = pipe.take(read_cap).read_to_end(&mut stderr_buf).await;
            }
        };
        let status = child.wait();
        let (_, _, _, status) = tokio::join!(write, read_out, read_err, status);
        status
    });
    let status = tokio::select! {
        status = timed => match status {
            Ok(Ok(status)) => status,
            Ok(Err(error)) => {
                return HookRaw {
                    outcome: RawOutcome::SpawnFailed(error.to_string()),
                    stdout: String::new(),
                    stderr: String::new(),
                };
            }
            Err(_) => {
                kill(pid);
                return HookRaw {
                    outcome: RawOutcome::TimedOut,
                    stdout: decode_capped(&mut stdout_buf, max_output_bytes),
                    stderr: decode_capped(&mut stderr_buf, max_output_bytes),
                };
            }
        },
        _ = run.cancel.cancelled() => {
            kill(pid);
            return HookRaw {
                outcome: RawOutcome::Cancelled,
                stdout: String::new(),
                stderr: String::new(),
            };
        }
    };
    HookRaw {
        outcome: RawOutcome::Exited(status.code()),
        stdout: decode_capped(&mut stdout_buf, max_output_bytes),
        stderr: decode_capped(&mut stderr_buf, max_output_bytes),
    }
}

/// A written transcript temp file; Drop removes it so the hook's view of the
/// conversation never outlives its own run
struct TranscriptFile(Option<std::path::PathBuf>);

impl TranscriptFile {
    fn write(lines: &[String]) -> Self {
        if lines.is_empty() {
            return Self(None);
        }
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "pig-hook-transcript-{}-{}.jsonl",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let mut body = lines.join("\n");
        body.push('\n');
        match std::fs::write(&path, body) {
            Ok(()) => Self(Some(path)),
            Err(error) => {
                tracing::warn!(detail = %error, "hook transcript write failed; running without transcript_path");
                Self(None)
            }
        }
    }

    fn path_str(&self) -> Option<&str> {
        self.0.as_ref().and_then(|path| path.to_str())
    }
}

impl Drop for TranscriptFile {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// kill_on_drop already reaps the direct child on drop; the explicit tree kill
/// covers group members spawned by the hook (same helper as task timeouts)
fn kill(pid: Option<u32>) {
    if let Some(pid) = pid {
        crate::task::kill_process_tree(pid);
    }
}

/// `${VAR}` expansion for hook commands/args: the injected hook variables
/// (PIG_* and their CLAUDE_* aliases) first, then the process environment;
/// an unset/unknown name stays literal so the shell can still see it
fn expand_vars(text: &str, session_id: &str, cwd: &std::path::Path) -> String {
    if !text.contains("${") {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find('}') {
            Some(end) => {
                let name = &after[..end];
                match var_value(name, session_id, cwd) {
                    Some(value) => out.push_str(&value),
                    None => out.push_str(&rest[start..start + 2 + end + 1]),
                }
                rest = &after[end + 1..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

fn var_value(name: &str, session_id: &str, cwd: &std::path::Path) -> Option<String> {
    match name {
        "PIG_SESSION_ID" | "CLAUDE_SESSION_ID" | "CLAUDE_CODE_SESSION_ID" => {
            Some(session_id.to_string())
        }
        "PIG_PROJECT_DIR" | "CLAUDE_PROJECT_DIR" => Some(cwd.display().to_string()),
        _ => std::env::var(name).ok(),
    }
}

/// Bytes → lossy UTF-8 string, cut to the cap on a char boundary
fn decode_capped(buf: &mut Vec<u8>, cap: usize) -> String {
    if buf.len() > cap {
        buf.truncate(cap);
        let mut text = String::from_utf8_lossy(buf).to_string();
        while !text.is_char_boundary(text.len()) {
            text.pop();
        }
        text.push_str("\n…[hook output truncated]");
        text
    } else {
        String::from_utf8_lossy(buf).to_string()
    }
}
