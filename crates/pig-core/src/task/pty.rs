//! Model-side persistent PTY sessions (Codex `UnifiedExec`-style).
//!
//! Backed by `portable-pty` (ConPTY on Windows, POSIX PTY on macOS/Linux).
//! Each session owns one pseudo-terminal and child process, registers a visible
//! entry in [`TaskRegistry`] (so `TaskList`/`TaskOutput`/`TaskStop` and session
//! deletion manage it uniformly), enforces the session sandbox via
//! [`super::shell::prepare_model_command`], and streams live output chunks to
//! the active [`OutputDeltaSink`].

use std::collections::HashMap;
use std::io::{Read as _, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pig_protocol::{TaskKind, TaskStatus};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::rollout::now_secs;

use super::notification::terminal_change;
use super::output::{OutputDeltaSink, current_output_stream};
use super::registry::{SessionToolState, TaskEntry, append_output, next_task_id, spill_for};
use super::sandbox::{AdditionalPermissions, SandboxOverride, SandboxProfile};
use super::shell::{ModelShell, prepare_model_command};

/// Maximum number of concurrent interactive PTY sessions per conversation.
pub const MAX_PTY_SESSIONS: usize = 8;
/// Default terminal geometry for model-driven PTYs.
const DEFAULT_PTY_COLS: u16 = 120;
const DEFAULT_PTY_ROWS: u16 = 36;
/// Default wait window when starting an interactive PTY command.
pub const DEFAULT_PTY_START_YIELD_MS: u64 = 5_000;
/// Default wait window when writing stdin / polling an interactive PTY session.
pub const DEFAULT_PTY_WRITE_YIELD_MS: u64 = 1_000;
const MIN_YIELD_MS: u64 = 50;
const MAX_YIELD_MS: u64 = 30_000;
/// Same 16 MiB total output hard stop as piped shell tasks.
const MAX_PTY_OUTPUT_TOTAL: usize = 16 * 1024 * 1024;
/// Rolling cap on unread incremental output buffered between model polls.
const MAX_PTY_UNREAD_BYTES: usize = 64 * 1024;
/// Maximum time to wait for the PTY reader thread to drain after child exit.
const EXIT_DRAIN_GRACE: Duration = Duration::from_millis(250);

/// Request to start a persistent PTY command.
#[derive(Clone, Debug)]
pub struct PtyStartRequest {
    pub workspace: PathBuf,
    pub workdir: PathBuf,
    pub command: String,
    pub shell: ModelShell,
    pub yield_time_ms: u64,
    pub sandbox_override: SandboxOverride,
    pub sandbox_profile: SandboxProfile,
    pub additional_permissions: AdditionalPermissions,
}

/// Result of starting or interacting with a PTY session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PtyOutcome {
    /// Process exited within the yield window; session has been cleaned up.
    Completed { output: String, code: i32 },
    /// Process is still running; caller may continue with `WriteStdin` or stop via `TaskStop`.
    Running {
        pty_session_id: String,
        task_id: String,
        output: String,
    },
}

struct PtySession {
    pty_session_id: String,
    task_id: String,
    writer: Mutex<Option<Box<dyn Write + Send>>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    unread: Mutex<String>,
    live_sink: Mutex<Option<OutputDeltaSink>>,
    exited: AtomicBool,
    reader_done: AtomicBool,
    capped: AtomicBool,
    exit_code: Mutex<Option<i32>>,
    notify: Arc<tokio::sync::Notify>,
}

impl PtySession {
    fn drain_unread(&self) -> String {
        let mut guard = self
            .unread
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        std::mem::take(&mut *guard)
    }

    fn set_live_sink(&self, sink: Option<OutputDeltaSink>) {
        *self
            .live_sink
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = sink;
    }

    fn terminate(&self) {
        if let Ok(mut killer) = self.killer.lock() {
            let _ = killer.kill();
        }
        // Dropping writer + master closes the PTY handle so any blocked reader
        // thread receives EOF and exits promptly.
        if let Ok(mut writer) = self.writer.lock() {
            writer.take();
        }
        if let Ok(mut master) = self.master.lock() {
            master.take();
        }
        self.notify.notify_waiters();
    }
}

/// RAII guard that attaches the active tool invocation's live output sink to a
/// PTY session and clears it as soon as the tool call returns or is cancelled.
struct LiveSinkGuard {
    session: Arc<PtySession>,
}

impl LiveSinkGuard {
    fn attach(session: &Arc<PtySession>, sink: Option<OutputDeltaSink>) -> Self {
        session.set_live_sink(sink);
        Self {
            session: Arc::clone(session),
        }
    }
}

impl Drop for LiveSinkGuard {
    fn drop(&mut self) {
        self.session.set_live_sink(None);
    }
}

/// Session-scoped manager for model-driven persistent PTY sessions.
#[derive(Default)]
pub struct PtySessionManager {
    sessions: Mutex<HashMap<String, Arc<PtySession>>>,
    seq: AtomicUsize,
}

impl Drop for PtySessionManager {
    fn drop(&mut self) {
        self.close_all();
    }
}

impl PtySessionManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of currently tracked live PTY sessions.
    pub fn active_count(&self) -> usize {
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }

    /// Start a command attached to a real PTY and wait up to `yield_time_ms`
    /// for either completion or initial interactive output.
    pub async fn start(
        self: &Arc<Self>,
        state: &SessionToolState,
        request: PtyStartRequest,
    ) -> Result<PtyOutcome, String> {
        self.prune_exited();
        {
            let sessions = self
                .sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if sessions.len() >= MAX_PTY_SESSIONS {
                return Err(format!(
                    "Maximum active PTY sessions ({MAX_PTY_SESSIONS}) reached; finish or stop an existing PTY session with WriteStdin/TaskStop before starting another"
                ));
            }
        }

        let sandbox_cmd = prepare_model_command(
            state,
            &request.workspace,
            &request.workdir,
            &request.command,
            &request.shell,
            request.sandbox_override,
            request.sandbox_profile,
            &request.additional_permissions,
        )
        .map_err(|error| format!("Failed to prepare PTY command: {error}"))?;

        let mut cmd = CommandBuilder::new(&sandbox_cmd.program);
        #[cfg(windows)]
        {
            // `portable_pty::CommandBuilder` quotes arguments with MSVC
            // (`CommandLineToArgvW`) rules, which escapes embedded `"` as `\"`.
            // `cmd.exe /C` does not unescape `\"`, so the sandbox helper's
            // `cd /d "<dir>" && ...` wrapper breaks when `cmd.exe` is spawned
            // directly (unsandboxed). `cmd.cwd(&request.workdir)` already sets
            // the working directory via `CreateProcessW`, so pass `/C <cmd>`
            // directly for unsandboxed `cmd.exe`.
            let is_direct_cmd = request.shell.kind == super::shell::ShellKind::Cmd
                && sandbox_cmd
                    .program
                    .to_string_lossy()
                    .to_ascii_lowercase()
                    .ends_with("cmd.exe");
            if is_direct_cmd {
                cmd.arg("/C");
                cmd.arg(super::shell::rewrite_nul_redirects(&request.command));
            } else {
                cmd.args(&sandbox_cmd.args);
            }
        }
        #[cfg(not(windows))]
        cmd.args(&sandbox_cmd.args);
        cmd.cwd(&request.workdir);
        for (key, value) in &sandbox_cmd.env {
            cmd.env(key, value);
        }
        cmd.env("TERM", "xterm-256color");
        cmd.env("GIT_TERMINAL_PROMPT", "0");
        cmd.env("PYTHONIOENCODING", "utf-8");
        cmd.env("PYTHONUTF8", "1");
        if std::env::var_os("LANG").is_none() && std::env::var_os("LC_ALL").is_none() {
            cmd.env("LANG", "C.UTF-8");
        }

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: DEFAULT_PTY_ROWS,
                cols: DEFAULT_PTY_COLS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| format!("Failed to open PTY: {error}"))?;

        let mut child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|error| format!("Failed to spawn PTY command: {error}"))?;
        // Drop the slave handle immediately so the master reader sees EOF when
        // the child exits.
        drop(pair.slave);

        let pid = child.process_id();
        let killer = child.clone_killer();
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|error| format!("Failed to clone PTY reader: {error}"))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|error| format!("Failed to open PTY writer: {error}"))?;

        let pty_session_id = format!("pty-{}", self.seq.fetch_add(1, Ordering::Relaxed) + 1);
        let cancel = tokio_util::sync::CancellationToken::new();
        let task_id = {
            let mut tasks = state.tasks.lock().expect("task registry lock");
            let id = next_task_id(&state.task_seq);
            tasks.push(TaskEntry {
                id: id.clone(),
                kind: TaskKind::Shell,
                command: format!("[pty:{pty_session_id}] {}", request.command),
                status: TaskStatus::Running,
                exit_code: None,
                reason: None,
                started_at: now_secs(),
                ended_at: None,
                pid,
                process: None,
                output: String::new(),
                spill: spill_for(state, &id),
                cancel: Some(cancel.clone()),
                agent_id: None,
                run: None,
                owner_agent_id: state.owner_agent_id.clone(),
                record_path: None,
                result_path: None,
                turns: None,
                model: None,
                terminal_notification_suppressed: false,
                foreground: true,
            });
            id
        };

        let session = Arc::new(PtySession {
            pty_session_id: pty_session_id.clone(),
            task_id: task_id.clone(),
            writer: Mutex::new(Some(writer)),
            killer: Mutex::new(killer),
            master: Mutex::new(Some(pair.master)),
            unread: Mutex::new(String::new()),
            live_sink: Mutex::new(None),
            exited: AtomicBool::new(false),
            reader_done: AtomicBool::new(false),
            capped: AtomicBool::new(false),
            exit_code: Mutex::new(None),
            notify: Arc::new(tokio::sync::Notify::new()),
        });

        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(pty_session_id.clone(), Arc::clone(&session));

        // Attach the active tool call's live stream before spawning the reader
        // thread so early chunks are streamed immediately.
        let _sink_guard = LiveSinkGuard::attach(&session, current_output_stream());

        // Cancel bridge: `TaskStop` cancels the task's token, which terminates
        // the PTY cleanly and removes it from the active map.
        {
            let manager = Arc::clone(self);
            let sid = pty_session_id.clone();
            let session_ref = Arc::clone(&session);
            tokio::spawn(async move {
                cancel.cancelled().await;
                session_ref.terminate();
                manager.remove(&sid);
            });
        }

        // Reader thread: decodes PTY output, feeds TaskRegistry/spill, appends
        // to the incremental unread buffer, and streams to the live sink.
        {
            let session_ref = Arc::clone(&session);
            let registry = state.tasks.clone();
            let tid = task_id.clone();
            std::thread::Builder::new()
                .name(format!("pig-pty-read-{pty_session_id}"))
                .spawn(move || {
                    read_pty_loop(reader, session_ref, registry, tid);
                })
                .map_err(|error| format!("Failed to spawn PTY reader thread: {error}"))?;
        }

        // Exit watcher thread: waits for child termination and settles the
        // TaskRegistry entry.
        {
            let session_ref = Arc::clone(&session);
            let registry = state.tasks.clone();
            let notify_tx = state.task_notify.clone();
            let root_session_id = state.session_id.clone();
            let tid = task_id.clone();
            std::thread::Builder::new()
                .name(format!("pig-pty-wait-{pty_session_id}"))
                .spawn(move || {
                    let code = match child.wait() {
                        Ok(status) => status.exit_code() as i32,
                        Err(_) => -1,
                    };
                    *session_ref
                        .exit_code
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(code);
                    session_ref.exited.store(true, Ordering::Release);
                    let change = {
                        let mut tasks = registry.lock().expect("task registry lock");
                        tasks.iter_mut().find(|entry| entry.id == tid).map(|entry| {
                            if matches!(entry.status, TaskStatus::Running) {
                                entry.status = if code == 0 {
                                    TaskStatus::Completed
                                } else {
                                    TaskStatus::Failed
                                };
                                entry.exit_code = Some(code);
                                entry.ended_at = Some(now_secs());
                            }
                            (!entry.foreground).then(|| terminal_change(&root_session_id, entry))
                        })
                    };
                    if let Some(Some(change)) = change {
                        super::notification::publish_change(&notify_tx, change);
                    }
                    session_ref.notify.notify_waiters();
                })
                .map_err(|error| format!("Failed to spawn PTY waiter thread: {error}"))?;
        }

        let yield_ms = request.yield_time_ms.clamp(MIN_YIELD_MS, MAX_YIELD_MS);
        let outcome = self.wait_for_outcome(state, &session, yield_ms).await;
        if matches!(outcome, PtyOutcome::Running { .. }) {
            // Expose the live persistent PTY in TaskRegistry snapshots so the
            // user and model can inspect or stop it via TaskList/TaskStop.
            {
                let mut tasks = state.tasks.lock().expect("task registry lock");
                if let Some(entry) = tasks.iter_mut().find(|entry| entry.id == task_id) {
                    entry.foreground = false;
                }
            }
            state.notify_task(&task_id);
        }
        Ok(outcome)
    }

    /// Write input characters (or an empty string to poll output) to a running
    /// PTY session and wait up to `yield_time_ms` for incremental output or exit.
    pub async fn write_stdin(
        self: &Arc<Self>,
        state: &SessionToolState,
        pty_session_id: &str,
        chars: &str,
        yield_time_ms: u64,
    ) -> Result<PtyOutcome, String> {
        let session = {
            let sessions = self
                .sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            sessions.get(pty_session_id).cloned()
        }
        .ok_or_else(|| {
            format!("Unknown or already exited PTY session: {pty_session_id}")
        })?;

        let _sink_guard = LiveSinkGuard::attach(&session, current_output_stream());

        if !chars.is_empty() {
            if session.exited.load(Ordering::Acquire) {
                // Process already exited before we wrote; fall through to drain
                // final output and return Completed.
            } else {
                let mut writer_guard = session
                    .writer
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let writer = writer_guard.as_mut().ok_or_else(|| {
                    format!("PTY session {pty_session_id} input stream is closed")
                })?;
                writer
                    .write_all(chars.as_bytes())
                    .and_then(|()| writer.flush())
                    .map_err(|error| {
                        format!("Failed to write to PTY session {pty_session_id}: {error}")
                    })?;
            }
        }

        let yield_ms = yield_time_ms.clamp(MIN_YIELD_MS, MAX_YIELD_MS);
        Ok(self.wait_for_outcome(state, &session, yield_ms).await)
    }

    /// Terminate one PTY session by its `pty_session_id`.
    pub fn stop(&self, pty_session_id: &str) -> bool {
        if let Some(session) = self.remove(pty_session_id) {
            session.terminate();
            true
        } else {
            false
        }
    }

    /// Terminate and clear all active PTY sessions (called on session teardown
    /// and `Drop`).
    pub fn close_all(&self) {
        let drained: Vec<Arc<PtySession>> = {
            let mut sessions = self
                .sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            sessions.drain().map(|(_, session)| session).collect()
        };
        for session in drained {
            session.terminate();
        }
    }

    fn remove(&self, pty_session_id: &str) -> Option<Arc<PtySession>> {
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(pty_session_id)
    }

    fn prune_exited(&self) {
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        sessions.retain(|_, session| !session.exited.load(Ordering::Acquire));
    }

    async fn wait_for_outcome(
        &self,
        state: &SessionToolState,
        session: &Arc<PtySession>,
        yield_ms: u64,
    ) -> PtyOutcome {
        let deadline = Instant::now() + Duration::from_millis(yield_ms);
        loop {
            if session.exited.load(Ordering::Acquire) {
                break;
            }
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let notified = session.notify.notified();
            if session.exited.load(Ordering::Acquire) {
                break;
            }
            let remaining = deadline.saturating_duration_since(now);
            let _ = tokio::time::timeout(remaining, notified).await;
        }

        if session.exited.load(Ordering::Acquire) {
            // Give the reader thread a brief grace window to flush trailing
            // bytes written right before exit.
            let drain_deadline = Instant::now() + EXIT_DRAIN_GRACE;
            while !session.reader_done.load(Ordering::Acquire) {
                let now = Instant::now();
                if now >= drain_deadline {
                    break;
                }
                let notified = session.notify.notified();
                if session.reader_done.load(Ordering::Acquire) {
                    break;
                }
                let _ = tokio::time::timeout(drain_deadline - now, notified).await;
            }
            let mut output = session.drain_unread();
            if session.capped.load(Ordering::Relaxed) {
                output.push_str(
                    "\n\n[Output exceeded the 16MiB limit; the PTY session was force-stopped]",
                );
            }
            let code = session
                .exit_code
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .unwrap_or(-1);
            // Clean up finished session; if it finished during the initial
            // foreground window, remove the temporary foreground registry entry
            // just like piped `run_foreground_with_profile`.
            self.remove(&session.pty_session_id);
            session.terminate();
            {
                let mut tasks = state.tasks.lock().expect("task registry lock");
                if tasks
                    .iter()
                    .any(|entry| entry.id == session.task_id && entry.foreground)
                {
                    tasks.retain(|entry| entry.id != session.task_id);
                }
            }
            return PtyOutcome::Completed { output, code };
        }

        let output = session.drain_unread();
        PtyOutcome::Running {
            pty_session_id: session.pty_session_id.clone(),
            task_id: session.task_id.clone(),
            output,
        }
    }
}

fn read_pty_loop(
    mut reader: Box<dyn std::io::Read + Send>,
    session: Arc<PtySession>,
    registry: super::registry::TaskRegistry,
    task_id: String,
) {
    const DSR_CURSOR_QUERY: &[u8] = b"\x1b[6n";
    const DSR_CURSOR_REPLY: &[u8] = b"\x1b[1;1R";
    const DA1_DEVICE_QUERY: &[u8] = b"\x1b[c";
    const DA1_DEVICE_REPLY: &[u8] = b"\x1b[?1;0c";

    let mut decoder = pig_utils::text::StreamDecoder::new();
    let mut buf = [0u8; 4096];
    let mut dsr_tail: Vec<u8> = Vec::new();
    let mut da1_tail: Vec<u8> = Vec::new();
    let mut total_bytes = 0usize;
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                // Answer terminal capability/cursor queries (`CSI 6 n`, `CSI c`)
                // automatically: Windows ConPTY and interactive CLIs emit them
                // during startup and block waiting for a terminal emulator reply.
                let chunk = &buf[..n];
                let dsr_hits = count_dsr_queries(&mut dsr_tail, chunk, DSR_CURSOR_QUERY);
                let da1_hits = count_dsr_queries(&mut da1_tail, chunk, DA1_DEVICE_QUERY);
                if (dsr_hits > 0 || da1_hits > 0)
                    && let Ok(mut writer_guard) = session.writer.lock()
                    && let Some(writer) = writer_guard.as_mut()
                {
                    for _ in 0..dsr_hits {
                        let _ = writer.write_all(DSR_CURSOR_REPLY);
                    }
                    for _ in 0..da1_hits {
                        let _ = writer.write_all(DA1_DEVICE_REPLY);
                    }
                    let _ = writer.flush();
                }

                let text = decoder.push(chunk);
                if !text.is_empty() {
                    append_output(&registry, &task_id, chunk, &text);
                    push_unread(&session, &text);
                    emit_pty_live(&session, &text);
                    session.notify.notify_waiters();
                } else {
                    append_output(&registry, &task_id, chunk, "");
                }
                total_bytes = total_bytes.saturating_add(n);
                if total_bytes > MAX_PTY_OUTPUT_TOTAL {
                    session.capped.store(true, Ordering::Relaxed);
                    session.terminate();
                    break;
                }
            }
        }
    }
    let tail = decoder.finish();
    if !tail.is_empty() {
        append_output(&registry, &task_id, b"", &tail);
        push_unread(&session, &tail);
        emit_pty_live(&session, &tail);
    }
    session.reader_done.store(true, Ordering::Release);
    session.notify.notify_waiters();
}

/// Count occurrences of `needle` across streaming byte chunks, carrying a small
/// suffix buffer so a split escape sequence across two `read()` calls is still
/// recognized.
fn count_dsr_queries(carry: &mut Vec<u8>, chunk: &[u8], needle: &[u8]) -> usize {
    if needle.is_empty() {
        return 0;
    }
    let mut combined = Vec::with_capacity(carry.len() + chunk.len());
    combined.extend_from_slice(carry);
    combined.extend_from_slice(chunk);
    let mut count = 0usize;
    let mut idx = 0usize;
    while idx + needle.len() <= combined.len() {
        if &combined[idx..idx + needle.len()] == needle {
            count += 1;
            idx += needle.len();
        } else {
            idx += 1;
        }
    }
    let keep = needle.len().saturating_sub(1);
    carry.clear();
    if combined.len() >= keep {
        carry.extend_from_slice(&combined[combined.len() - keep..]);
    } else {
        carry.extend_from_slice(&combined);
    }
    count
}

fn push_unread(session: &PtySession, text: &str) {
    let mut unread = session
        .unread
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    unread.push_str(text);
    if unread.len() > MAX_PTY_UNREAD_BYTES {
        let mut start = unread.len() - MAX_PTY_UNREAD_BYTES;
        while start < unread.len() && !unread.is_char_boundary(start) {
            start += 1;
        }
        unread.drain(..start);
    }
}

fn emit_pty_live(session: &PtySession, text: &str) {
    let sink = session
        .live_sink
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    if let Some(sink) = sink {
        sink(text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::output::with_tool_output_stream;

    #[tokio::test]
    async fn interactive_pty_accepts_write_stdin_and_streams_live_output() {
        let state = SessionToolState::for_test();
        let workspace = std::env::temp_dir();
        let shell = if cfg!(windows) {
            crate::task::resolve_model_shell(Some("cmd"), None).expect("cmd shell")
        } else {
            crate::task::resolve_model_shell(Some("sh"), None).expect("sh shell")
        };
        let command = if cfg!(windows) {
            "set /p REPLY=PROMPT: && cmd /v:on /c echo GOT-!REPLY!"
        } else {
            "printf 'PROMPT: '; read REPLY; echo \"GOT-$REPLY\""
        };

        let streamed = Arc::new(Mutex::new(String::new()));
        let streamed_clone = Arc::clone(&streamed);
        let sink: OutputDeltaSink = Arc::new(move |chunk: &str| {
            streamed_clone.lock().unwrap().push_str(chunk);
        });

        let started = with_tool_output_stream(
            sink.clone(),
            state.pty.start(
                &state,
                PtyStartRequest {
                    workspace: workspace.clone(),
                    workdir: workspace.clone(),
                    command: command.to_string(),
                    shell,
                    yield_time_ms: 1_500,
                    sandbox_override: SandboxOverride::NoOverride,
                    sandbox_profile: SandboxProfile::Unrestricted,
                    additional_permissions: AdditionalPermissions::default(),
                },
            ),
        )
        .await
        .expect("pty start");

        let pty_id = match started {
            PtyOutcome::Running {
                pty_session_id,
                task_id,
                output,
            } => {
                assert!(!task_id.is_empty());
                assert!(
                    output.contains("PROMPT:")
                        || streamed.lock().unwrap().contains("PROMPT:"),
                    "expected interactive prompt, got output={output:?}, streamed={:?}",
                    *streamed.lock().unwrap()
                );
                pty_session_id
            }
            PtyOutcome::Completed { output, code } => {
                panic!("expected interactive PTY to stay running, got exit {code}: {output:?}")
            }
        };

        assert_eq!(state.pty.active_count(), 1);

        let reply = with_tool_output_stream(
            sink,
            state.pty.write_stdin(&state, &pty_id, "pig42\r\n", 4_000),
        )
        .await
        .expect("write_stdin");

        match reply {
            PtyOutcome::Completed { output, code } => {
                assert_eq!(code, 0, "unexpected exit code with output: {output:?}");
                assert!(output.contains("GOT-pig42"), "output={output:?}");
            }
            PtyOutcome::Running { output, .. } => {
                state.pty.stop(&pty_id);
                panic!("expected PTY to exit after stdin line, still running with output={output:?}");
            }
        }

        assert_eq!(state.pty.active_count(), 0);
        assert!(
            streamed.lock().unwrap().contains("GOT-pig42"),
            "live sink should receive streamed PTY chunks: {:?}",
            *streamed.lock().unwrap()
        );
    }

    #[tokio::test]
    async fn pty_close_all_stops_running_sessions() {
        let state = SessionToolState::for_test();
        let workspace = std::env::temp_dir();
        let shell = if cfg!(windows) {
            crate::task::resolve_model_shell(Some("cmd"), None).expect("cmd shell")
        } else {
            crate::task::resolve_model_shell(Some("sh"), None).expect("sh shell")
        };
        let command = if cfg!(windows) {
            "set /p WAIT=waiting:"
        } else {
            "read WAIT"
        };

        let outcome = state
            .pty
            .start(
                &state,
                PtyStartRequest {
                    workspace: workspace.clone(),
                    workdir: workspace,
                    command: command.to_string(),
                    shell,
                    yield_time_ms: 250,
                    sandbox_override: SandboxOverride::NoOverride,
                    sandbox_profile: SandboxProfile::Unrestricted,
                    additional_permissions: AdditionalPermissions::default(),
                },
            )
            .await
            .expect("pty start");
        assert!(matches!(outcome, PtyOutcome::Running { .. }));
        assert_eq!(state.pty.active_count(), 1);

        state.pty.close_all();
        assert_eq!(state.pty.active_count(), 0);
    }
}
