//! Session-scoped Bash tasks (one unified registry for foreground/background):
//! readers collect output, the watcher waits for exit and updates status, and
//! completion is reported to agent_loop via the task_notify channel to push
//! TaskListChanged. Not persisted. Foreground tasks that time out automatically
//! continue in the background; the spill file (.pigcode/tool-results/{id}.log)
//! keeps the full output. Entries still running in the foreground are hidden
//! from snapshots (a plain command is not a background task); foreground is
//! flipped only when a timeout converts it to a background task, making it
//! visible on screen.

use crate::NoConsoleExt as _;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pig_protocol::{TaskStatus, TaskSummary};
use tokio::sync::mpsc::UnboundedSender;

use crate::rollout::now_secs;
use crate::tool::TodoHandle;

/// File freshness state recorded by Read (same as ZCode read-file-state):
/// Write/Edit compares it before writing, preventing external changes from
/// being overwritten based on a stale view.
#[derive(Debug, Clone)]
pub struct ReadState {
    pub mtime: Option<std::time::SystemTime>,
    pub size: u64,
    /// DefaultHasher over the raw bytes
    pub hash: u64,
    /// Whether this read was truncated by the budget (an incomplete view); explicit offset/limit paged reads do not count
    pub partial: bool,
    /// View parameters of the last successful Read (offset, limit, column_offset):
    /// re-reading with the same parameters and an unchanged hash short-circuits
    /// with "file unchanged" to save tokens. Internal refreshes done by
    /// Write/Edit/check_fresh carry no view (None).
    pub view: Option<(usize, usize, usize)>,
}

/// Rolling cap on in-registry output (truncated from the head on append).
const MAX_TASK_OUTPUT: usize = 64 * 1024;
/// Number of tail characters for output_tail in panel snapshots.
const SNAPSHOT_TAIL_CHARS: usize = 4000;
/// Spill file cap (full output persisted to disk) of 10MB; writing stops beyond it.
const MAX_SPILL_BYTES: u64 = 10 * 1024 * 1024;
/// Total accumulated output cap per task (stdout+stderr combined): exceeding it
/// force-stops the task — guards against "output-spewing commands" filling the
/// foreground sink memory within the timeout window, or running forever after
/// going to the background (same 16MiB hard kill as kimi-code; the spill 10MB
/// persist cap is independent of this).
const MAX_TASK_OUTPUT_TOTAL: usize = 16 * 1024 * 1024;

pub struct TaskEntry {
    pub id: String,
    pub command: String,
    pub status: TaskStatus,
    pub started_at: u64,
    pub ended_at: Option<u64>,
    pub pid: Option<u32>,
    pub output: String,
    /// Path where the full output is persisted (.pigcode/tool-results/{id}.log)
    pub spill_path: Option<PathBuf>,
    /// Driver cancellation token for subagent background tasks (always None for Bash); stop_task prefers it over killing the process tree
    pub cancel: Option<tokio_util::sync::CancellationToken>,
    /// agent_id of subagent background tasks (always None for Bash); used for resume-while-running conflict detection
    pub agent_id: Option<String>,
    /// A plain command running in the foreground: hidden from panel snapshots
    /// (it is not a background task); flipped to false only when a timeout
    /// converts it to a background task, after which it is visible to snapshots
    pub foreground: bool,
}

/// Per-session order-preserving task registry (id = b{incrementing task_seq}; removing an entry does not recycle its sequence number).
pub type TaskRegistry = Arc<Mutex<Vec<TaskEntry>>>;

/// Session-scoped shared tool state: todo list + task registry + completion
/// notification + wake channel + task sequence + file freshness. All fields are
/// Clone-able (Arc/atomic/sender); Session and agent_loop's SessionEntry each
/// hold one shared copy.
pub struct SessionToolState {
    pub todos: TodoHandle,
    pub tasks: TaskRegistry,
    /// The watcher sends the session_id once a task finishes; agent_loop pushes TaskListChanged accordingly
    pub task_notify: UnboundedSender<String>,
    /// Wake channel for completed background subagents: (session_id, notification text); agent_loop synthesizes a user message to start a new turn
    pub wake_notify: UnboundedSender<(String, String)>,
    pub session_id: String,
    /// Task sequence generator (b1, b2... monotonically increasing; not reused after foreground tasks are removed on completion)
    pub task_seq: Arc<AtomicUsize>,
    /// File freshness registered by Read (key = full path after resolve_checked)
    pub read_states: Arc<Mutex<HashMap<PathBuf, ReadState>>>,
    /// Session-level switch: allow reading files outside the workspace (the tmp directory is always allowed; sensitive files are always blocked)
    pub fs_read_outside: Arc<AtomicBool>,
    /// Session-level switch: allow writing files outside the workspace
    pub fs_write_outside: Arc<AtomicBool>,
    /// Extra always-readable roots (canonicalized when possible at
    /// construction): the data_dir/sessions subtree (subagent result.md/context
    /// jsonl), exempted for Read alongside tmp; read-only, never writable
    pub extra_read_roots: Vec<PathBuf>,
}

impl Clone for SessionToolState {
    fn clone(&self) -> Self {
        Self {
            todos: self.todos.clone(),
            tasks: self.tasks.clone(),
            task_notify: self.task_notify.clone(),
            wake_notify: self.wake_notify.clone(),
            session_id: self.session_id.clone(),
            task_seq: self.task_seq.clone(),
            read_states: self.read_states.clone(),
            fs_read_outside: self.fs_read_outside.clone(),
            fs_write_outside: self.fs_write_outside.clone(),
            extra_read_roots: self.extra_read_roots.clone(),
        }
    }
}

impl SessionToolState {
    pub fn new(
        session_id: String,
        task_notify: UnboundedSender<String>,
        wake_notify: UnboundedSender<(String, String)>,
        extra_read_roots: Vec<PathBuf>,
    ) -> Self {
        Self {
            todos: TodoHandle::default(),
            tasks: TaskRegistry::default(),
            task_notify,
            wake_notify,
            session_id,
            task_seq: Arc::new(AtomicUsize::new(0)),
            read_states: Arc::new(Mutex::new(HashMap::new())),
            fs_read_outside: Arc::new(AtomicBool::new(false)),
            fs_write_outside: Arc::new(AtomicBool::new(false)),
            // Canonicalize when possible (fall back to the original path if the directory does not exist): allowlist comparison is done in canonical terms
            extra_read_roots: extra_read_roots
                .into_iter()
                .map(|root| root.canonicalize().unwrap_or(root))
                .collect(),
        }
    }

    /// For tests: session_id = "test"; notify/wake receivers are dropped
    /// outright (send failures ignored). extra_read_roots gets the tmp directory
    /// (tmp is already exempt, so existing test behavior is unchanged).
    pub fn for_test() -> Self {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (wake_tx, _wake_rx) = tokio::sync::mpsc::unbounded_channel();
        Self::new("test".to_string(), tx, wake_tx, vec![std::env::temp_dir()])
    }
}

/// The last max characters of the output (truncated at a char boundary).
pub fn tail_chars(text: &str, max: usize) -> String {
    let total = text.chars().count();
    if total <= max {
        return text.to_string();
    }
    text.chars().skip(total - max).collect()
}

/// Panel snapshot: output_tail takes the last 4000 characters; entries still
/// running in the foreground are not shown (a plain command is not a background
/// task; this prevents an arbitrary notify during execution from surfacing it
/// as "Background Bash · Running").
pub fn snapshot(registry: &TaskRegistry) -> Vec<TaskSummary> {
    let tasks = registry.lock().expect("task registry lock");
    tasks
        .iter()
        .filter(|entry| !entry.foreground)
        .map(|entry| TaskSummary {
            id: entry.id.clone(),
            command: entry.command.clone(),
            status: entry.status,
            started_at: entry.started_at,
            ended_at: entry.ended_at,
            output_tail: tail_chars(&entry.output, SNAPSHOT_TAIL_CHARS),
            agent_id: entry.agent_id.clone(),
        })
        .collect()
}

/// Task sequence: b1, b2... monotonically increasing (foreground tasks are removed from the registry on completion, so len+1 risks reuse)
fn next_task_id(seq: &AtomicUsize) -> String {
    format!("b{}", seq.fetch_add(1, Ordering::Relaxed) + 1)
}

/// Uniform spill path assignment for registered tasks: {cwd}/.pigcode/tool-results/{task_id}.log
fn spill_path_for(cwd: &Path, task_id: &str) -> PathBuf {
    cwd.join(".pigcode")
        .join("tool-results")
        .join(format!("{task_id}.log"))
}

/// Roll the registry output (unchanged 64KB head truncation); also append to
/// the spill file when spill_path is set. raw goes to the spill (byte-faithful,
/// including incomplete suspended sequences); text is the decoded view
/// (produced by StreamDecoder, falling back to GBK for non-UTF-8 output on
/// Windows).
fn append_output(registry: &TaskRegistry, task_id: &str, raw: &[u8], text: &str) {
    let spill = {
        let mut tasks = registry.lock().expect("task registry lock");
        let Some(entry) = tasks.iter_mut().find(|t| t.id == task_id) else {
            return;
        };
        entry.output.push_str(text);
        if entry.output.len() > MAX_TASK_OUTPUT {
            let mut start = entry.output.len() - MAX_TASK_OUTPUT;
            while start < entry.output.len() && !entry.output.is_char_boundary(start) {
                start += 1;
            }
            entry.output.drain(..start);
        }
        entry.spill_path.clone()
    };
    if let Some(path) = spill {
        append_spill(&path, raw);
    }
}

/// Spill append write (opened/closed per chunk): writing stops past 10MB, and the notice is appended only once at the truncation moment.
fn append_spill(path: &Path, chunk: &[u8]) {
    use std::io::Write as _;
    let len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if len >= MAX_SPILL_BYTES {
        return;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return;
    };
    let remaining = (MAX_SPILL_BYTES - len) as usize;
    if chunk.len() <= remaining {
        let _ = file.write_all(chunk);
    } else {
        let _ = file.write_all(&chunk[..remaining]);
        let _ = file.write_all("[Output exceeded 10MB; the rest was discarded]".as_bytes());
    }
}

/// Append text to entry.output (same 64KB head truncation; no spill write) — used for subagent progress/results.
pub fn note_output(registry: &TaskRegistry, task_id: &str, line: &str) {
    let mut tasks = registry.lock().expect("task registry lock");
    let Some(entry) = tasks.iter_mut().find(|t| t.id == task_id) else {
        return;
    };
    entry.output.push_str(line);
    if entry.output.len() > MAX_TASK_OUTPUT {
        let mut start = entry.output.len() - MAX_TASK_OUTPUT;
        while start < entry.output.len() && !entry.output.is_char_boundary(start) {
            start += 1;
        }
        entry.output.drain(..start);
    }
}

/// Register a subagent background task entry (Running, no pid/spill; cancel =
/// the driver cancellation token, agent_id for resume-while-running conflict
/// detection and panel labeling), notify immediately so the panel shows the
/// entry, and return the task_id.
pub fn register_agent_task(
    state: &SessionToolState,
    command: String,
    cancel: tokio_util::sync::CancellationToken,
    agent_id: String,
) -> String {
    let id = {
        let mut tasks = state.tasks.lock().expect("task registry lock");
        let id = next_task_id(&state.task_seq);
        tasks.push(TaskEntry {
            id: id.clone(),
            command,
            status: TaskStatus::Running,
            started_at: now_secs(),
            ended_at: None,
            pid: None,
            output: String::new(),
            spill_path: None,
            cancel: Some(cancel),
            agent_id: Some(agent_id),
            foreground: false,
        });
        id
    };
    let _ = state.task_notify.send(state.session_id.clone());
    id
}

/// Global subagent concurrency cap (semaphore slots shared between background
/// Agent and the subagents fanned out by AgentSwarm). Making it configurable
/// would require touching pig-protocol's AppConfig (cross-crate); a constant
/// for v1.
pub const MAX_CONCURRENT_SUBAGENTS: usize = 8;

/// Global subagent concurrency slots: a process-level static (shared across sessions); over-limit requests queue at acquire.
static SUBAGENT_SLOTS: tokio::sync::Semaphore =
    tokio::sync::Semaphore::const_new(MAX_CONCURRENT_SUBAGENTS);

/// Number of currently free concurrency slots (for queueing hints; a transient value that may differ from the subsequent acquire result)
pub fn subagent_slots_available() -> usize {
    SUBAGENT_SLOTS.available_permits()
}

/// Acquire one subagent concurrency slot: over-limit requests queue; cancel
/// firing while waiting returns None (giving up the queue without taking a
/// slot). The permit is an RAII guard: dropping it returns the slot.
pub async fn acquire_subagent_slot(
    cancel: &tokio_util::sync::CancellationToken,
) -> Option<tokio::sync::SemaphorePermit<'static>> {
    tokio::select! {
        permit = SUBAGENT_SLOTS.acquire() => permit.ok(),
        _ = cancel.cancelled() => None,
    }
}

/// Register a "queued" subagent task: same shape as register_agent_task, with
/// the command carrying a "Queued · " prefix (TaskStatus has no Pending variant
/// and the protocol stays untouched — TaskList/panel show the queued state via
/// the command text); mark_agent_task_started strips the prefix once the
/// concurrency slot is acquired.
pub fn register_agent_task_queued(
    state: &SessionToolState,
    command: String,
    cancel: tokio_util::sync::CancellationToken,
    agent_id: String,
) -> String {
    register_agent_task(state, format!("Queued · {command}"), cancel, agent_id)
}

/// Concurrency slot acquired: strip the "Queued · " prefix and notify to
/// refresh the panel (started_at stays; elapsed time includes queueing — how
/// long it queued is part of the wait cost anyway).
pub fn mark_agent_task_started(state: &SessionToolState, task_id: &str) {
    {
        let mut tasks = state.tasks.lock().expect("task registry lock");
        if let Some(entry) = tasks.iter_mut().find(|t| t.id == task_id)
            && let Some(rest) = entry.command.strip_prefix("Queued · ")
        {
            entry.command = rest.to_string();
        }
    }
    let _ = state.task_notify.send(state.session_id.clone());
}

/// Accumulated task output metering (shared by the two stdout/stderr readers): crossing the total cap triggers a force stop only once.
#[derive(Default)]
struct OutputMeter {
    total: AtomicUsize,
    capped: AtomicBool,
}

impl OutputMeter {
    /// Add n bytes; returns true meaning "crossed the cap this time and not previously flagged", so the caller performs the one-time force stop.
    fn add(&self, n: usize) -> bool {
        let prev = self.total.fetch_add(n, Ordering::Relaxed);
        prev + n > MAX_TASK_OUTPUT_TOTAL && !self.capped.swap(true, Ordering::Relaxed)
    }
}

/// One-time force stop for exceeding the output cap: set Killed (the watcher
/// writes Exited only when still Running, so it will not overwrite), append the
/// notice to output (the rolling window keeps the tail, so the notice ends up
/// last), and kill the process tree.
fn cap_kill(registry: &TaskRegistry, task_id: &str) {
    let pid = {
        let mut tasks = registry.lock().expect("task registry lock");
        let Some(entry) = tasks.iter_mut().find(|t| t.id == task_id) else {
            return;
        };
        if matches!(entry.status, TaskStatus::Running) {
            entry.status = TaskStatus::Killed;
            entry.ended_at = Some(now_secs());
        }
        entry
            .output
            .push_str("\n[Output exceeded the 16MiB limit; the task was force-stopped. Redirect large output to a file (e.g. command > out.txt) and process it with Read/Grep]");
        entry.pid
    };
    if let Some(pid) = pid {
        kill_process_tree(pid);
    }
}

/// Kill the process tree: Windows taskkill /T (children included); unix kills
/// the process group first, then the pid as a fallback (spawn_shell's
/// process_group(0) makes the child its own group leader).
fn kill_process_tree(pid: u32) {
    if cfg!(target_os = "windows") {
        let _ = std::process::Command::new("taskkill")
            .no_console()
            .args(["/PID", &pid.to_string(), "/F", "/T"])
            .output();
    } else {
        let _ = std::process::Command::new("kill")
            .args(["-9", "--", &format!("-{pid}")])
            .output();
        let _ = std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .output();
    }
}

/// Pipe reader: chunk → decode (StreamDecoder: UTF-8 first / Windows GBK
/// fallback / cross-chunk suspension) → registry rolling output + spill persist
/// (raw bytes); when the sink is non-empty, also keep a full copy there
/// (foreground Completed needs stdout/stderr rendered separately, while the
/// registry copy is the merged stream). Crossing the 16MiB total cap
/// force-stops the task and stops reading.
async fn read_into<R: tokio::io::AsyncRead + Unpin>(
    mut reader: R,
    registry: TaskRegistry,
    task_id: String,
    sink: Option<Arc<Mutex<String>>>,
    meter: Arc<OutputMeter>,
) {
    let mut decoder = crate::text::StreamDecoder::new();
    let mut buf = [0u8; 4096];
    loop {
        match tokio::io::AsyncReadExt::read(&mut reader, &mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let text = decoder.push(&buf[..n]);
                append_output(&registry, &task_id, &buf[..n], &text);
                if let Some(sink) = &sink {
                    sink.lock().expect("stream sink lock").push_str(&text);
                }
                if meter.add(n) {
                    cap_kill(&registry, &task_id);
                    break;
                }
            }
        }
    }
    // Stream ended: flush any leftover incomplete tail bytes (not to the spill — raw bytes were already persisted per chunk)
    let tail = decoder.finish();
    if !tail.is_empty() {
        append_output(&registry, &task_id, b"", &tail);
    }
}

mod shell;

pub(crate) use shell::*;
pub use shell::{WindowsShell, shell_label, windows_shell};

/// Watcher: wait for child exit + drain both readers → set Exited only when
/// the status is still Running (avoiding overwriting the Killed set earlier by
/// stop_task) → send the session_id via task_notify. Shared by spawn_background
/// and run_foreground's timeout-to-background path.
fn spawn_watcher(
    registry: TaskRegistry,
    notify: UnboundedSender<String>,
    session_id: String,
    task_id: String,
    mut child: tokio::process::Child,
    read_out: tokio::task::JoinHandle<()>,
    read_err: tokio::task::JoinHandle<()>,
) {
    tokio::spawn(async move {
        let status = child.wait().await;
        let _ = read_out.await;
        let _ = read_err.await;
        let code = status.ok().and_then(|s| s.code()).unwrap_or(-1);
        {
            let mut tasks = registry.lock().expect("task registry lock");
            if let Some(entry) = tasks.iter_mut().find(|t| t.id == task_id)
                && matches!(entry.status, TaskStatus::Running)
            {
                entry.status = TaskStatus::Exited(code);
                entry.ended_at = Some(now_secs());
            }
        }
        let _ = notify.send(session_id);
    });
}

/// Start a shell command in the background: register the entry (Running, pid,
/// spill path) and return the task_id immediately; readers concurrently collect
/// stdout/stderr (merged into the registry rolling output + spill persist), and
/// the watcher waits for exit to finish up.
pub fn spawn_background(state: &SessionToolState, cwd: &Path, command: &str) -> String {
    let spawned = spawn_shell(cwd, command);
    let task_id = {
        let mut tasks = state.tasks.lock().expect("task registry lock");
        let id = next_task_id(&state.task_seq);
        let (status, ended_at, pid, output) = match &spawned {
            Ok(child) => (TaskStatus::Running, None, child.id(), String::new()),
            Err(e) => (
                TaskStatus::Exited(-1),
                Some(now_secs()),
                None,
                format!("Failed to start: {e}"),
            ),
        };
        tasks.push(TaskEntry {
            id: id.clone(),
            command: command.to_string(),
            status,
            started_at: now_secs(),
            ended_at,
            pid,
            output,
            spill_path: Some(spill_path_for(cwd, &id)),
            cancel: None,
            agent_id: None,
            foreground: false,
        });
        id
    };
    // Notify right after registration: chip/panel show it immediately (start failures are also pushed so the failed entry stays visible)
    let _ = state.task_notify.send(state.session_id.clone());
    let Ok(mut child) = spawned else {
        return task_id;
    };
    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");
    let meter = Arc::new(OutputMeter::default());
    let read_out = tokio::spawn(read_into(
        stdout,
        state.tasks.clone(),
        task_id.clone(),
        None,
        meter.clone(),
    ));
    let read_err = tokio::spawn(read_into(
        stderr,
        state.tasks.clone(),
        task_id.clone(),
        None,
        meter,
    ));
    spawn_watcher(
        state.tasks.clone(),
        state.task_notify.clone(),
        state.session_id.clone(),
        task_id.clone(),
        child,
        read_out,
        read_err,
    );
    task_id
}

/// Cancellation cleanup for run_foreground: when the in-flight future is
/// dropped (user clicks stop, turn aborted), the process is already killed via
/// kill_on_drop; remove the registry entry and spill file without a trace, and
/// notify to refresh the panel. The normal-completion / timeout-to-background
/// paths disarm before returning (state set to None), so drop does nothing.
struct ForegroundCancelGuard {
    state: Option<SessionToolState>,
    task_id: String,
}

impl Drop for ForegroundCancelGuard {
    fn drop(&mut self) {
        let Some(state) = self.state.take() else {
            return;
        };
        let spill = {
            let mut tasks = state.tasks.lock().expect("task registry lock");
            // Only clean up entries still in the foreground state (already backgrounded/removed ones are not its business)
            tasks
                .iter()
                .position(|t| t.id == self.task_id && t.foreground)
                .map(|ix| tasks.remove(ix).spill_path)
        };
        if let Some(spill) = spill.flatten() {
            let _ = std::fs::remove_file(spill);
        }
        let _ = state.task_notify.send(state.session_id.clone());
    }
}

/// Foreground execution outcome.
pub enum ForegroundOutcome {
    /// Completed: output is assembled as "stdout + [stderr] section"; the
    /// foreground task has been removed from the registry (no trace). The
    /// registry output has a 64KB rolling cap; the spill file is the full copy
    /// (≤10MB).
    Completed {
        output: String,
        code: i32,
        spill_path: Option<PathBuf>,
    },
    /// Timed out: the command stays in the registry and keeps running (the watcher has taken over); the task_id can be queried and stopped
    TimedOut {
        task_id: String,
    },
    SpawnFailed {
        error: String,
    },
}

/// Run a shell command in the foreground: register a Running entry → readers
/// split stdout/stderr → wait for exit within the timeout. On completion, drain
/// the pipes for the full text (registry removed without a trace); on timeout,
/// the watcher takes over and it keeps running in the background; cancellation
/// mid-execution (future drop) is cleaned up by the guard, which removes the
/// entry so no "running" leftover remains.
pub async fn run_foreground(
    state: &SessionToolState,
    cwd: &Path,
    command: &str,
    timeout: std::time::Duration,
) -> ForegroundOutcome {
    let mut child = match spawn_shell(cwd, command) {
        Ok(child) => child,
        Err(error) => {
            return ForegroundOutcome::SpawnFailed {
                error: error.to_string(),
            };
        }
    };
    let task_id = {
        let mut tasks = state.tasks.lock().expect("task registry lock");
        let id = next_task_id(&state.task_seq);
        tasks.push(TaskEntry {
            id: id.clone(),
            command: command.to_string(),
            status: TaskStatus::Running,
            started_at: now_secs(),
            ended_at: None,
            pid: child.id(),
            output: String::new(),
            spill_path: Some(spill_path_for(cwd, &id)),
            cancel: None,
            agent_id: None,
            foreground: true,
        });
        id
    };
    let mut cancel_guard = ForegroundCancelGuard {
        state: Some(state.clone()),
        task_id: task_id.clone(),
    };
    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");
    let stdout_sink = Arc::new(Mutex::new(String::new()));
    let stderr_sink = Arc::new(Mutex::new(String::new()));
    let meter = Arc::new(OutputMeter::default());
    let read_out = tokio::spawn(read_into(
        stdout,
        state.tasks.clone(),
        task_id.clone(),
        Some(stdout_sink.clone()),
        meter.clone(),
    ));
    let read_err = tokio::spawn(read_into(
        stderr,
        state.tasks.clone(),
        task_id.clone(),
        Some(stderr_sink.clone()),
        meter.clone(),
    ));
    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(status) => {
            let code = status.ok().and_then(|s| s.code()).unwrap_or(-1);
            // Join the readers first to drain the pipes, then take the full text and remove the registry entry
            let _ = read_out.await;
            let _ = read_err.await;
            let spill_path = {
                let mut tasks = state.tasks.lock().expect("task registry lock");
                let spill_path = tasks
                    .iter()
                    .find(|t| t.id == task_id)
                    .and_then(|entry| entry.spill_path.clone());
                tasks.retain(|t| t.id != task_id);
                spill_path
            };
            // Normal completion: the guard is no longer responsible for cleanup
            cancel_guard.state = None;
            let stdout = std::mem::take(&mut *stdout_sink.lock().expect("stream sink lock"));
            let stderr = std::mem::take(&mut *stderr_sink.lock().expect("stream sink lock"));
            let mut output = stdout;
            if !stderr.is_empty() {
                if !output.is_empty() {
                    output.push('\n');
                }
                output.push_str("[stderr]\n");
                output.push_str(&stderr);
            }
            // Force-stopped for exceeding the total output cap: append an
            // explanation to the completed output tail (the registry notice was
            // removed together with the entry, so the model only sees the sink
            // full text here)
            if meter.capped.load(Ordering::Relaxed) {
                output.push_str(
                    "\n\n[Output exceeded the 16MiB limit; the command was force-stopped. Redirect large output to a file (e.g. command > out.txt) and process it with Read/Grep]",
                );
            }
            ForegroundOutcome::Completed {
                output,
                code,
                spill_path,
            }
        }
        Err(_) => {
            // Timeout converts to background: the entry goes from "foreground
            // hidden" to a proper background task (visible to snapshots); notify
            // immediately to show it on screen; the watcher takes over the child
            // and the readers (the sink lives with the readers until the process
            // exits)
            {
                let mut tasks = state.tasks.lock().expect("task registry lock");
                if let Some(entry) = tasks.iter_mut().find(|t| t.id == task_id) {
                    entry.foreground = false;
                }
            }
            // Now a proper background task: the guard is no longer responsible for cleanup
            cancel_guard.state = None;
            spawn_watcher(
                state.tasks.clone(),
                state.task_notify.clone(),
                state.session_id.clone(),
                task_id.clone(),
                child,
                read_out,
                read_err,
            );
            let _ = state.task_notify.send(state.session_id.clone());
            ForegroundOutcome::TimedOut { task_id }
        }
    }
}

/// Stop a task: not found → Err; not Running → Err "task already finished";
/// set Killed + ended_at first (to keep the watcher/driver from overwriting the
/// status). Subagent tasks (cancel token present) go through token
/// cancellation — the driver loop cleans up itself, with no process to kill;
/// Bash tasks kill the tree: unix kill -9 the whole process group first
/// (spawn_shell's process_group(0) makes the child its own group leader), then
/// kill -9 the direct pid as a fallback (harmless if the group is already
/// gone); Windows taskkill /F /T. Finally notify.
pub fn stop_task(
    registry: &TaskRegistry,
    task_id: &str,
    notify: &UnboundedSender<String>,
    session_id: &str,
) -> Result<String, String> {
    let (pid, cancel) = {
        let mut tasks = registry.lock().expect("task registry lock");
        let Some(entry) = tasks.iter_mut().find(|t| t.id == task_id) else {
            return Err(format!("Task not found: {task_id}"));
        };
        if !matches!(entry.status, TaskStatus::Running) {
            return Err(format!("Task already finished: {task_id}"));
        }
        entry.status = TaskStatus::Killed;
        entry.ended_at = Some(now_secs());
        (entry.pid, entry.cancel.clone())
    };
    if let Some(token) = cancel {
        token.cancel();
    } else if let Some(pid) = pid {
        kill_process_tree(pid);
    }
    let _ = notify.send(session_id.to_string());
    Ok(format!("Stopped task {task_id}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Queued registration: the command carries the prefix; the prefix is stripped once the slot is acquired; an unknown task_id does not panic
    #[test]
    fn queued_registration_marks_pending() {
        let state = SessionToolState::for_test();
        let task_id = register_agent_task_queued(
            &state,
            "subagent explore: investigate the issue".to_string(),
            tokio_util::sync::CancellationToken::new(),
            "a1-1".to_string(),
        );
        {
            let tasks = state.tasks.lock().expect("task registry lock");
            let entry = tasks
                .iter()
                .find(|t| t.id == task_id)
                .expect("entry exists");
            assert!(
                entry.command.starts_with("Queued · "),
                "the queued state is shown via the command prefix: {}",
                entry.command
            );
            assert!(matches!(entry.status, TaskStatus::Running));
        }
        mark_agent_task_started(&state, &task_id);
        {
            let tasks = state.tasks.lock().expect("task registry lock");
            let entry = tasks
                .iter()
                .find(|t| t.id == task_id)
                .expect("entry exists");
            assert_eq!(
                entry.command, "subagent explore: investigate the issue",
                "the prefix is stripped once started"
            );
        }
        mark_agent_task_started(&state, "b999");
    }

    /// Concurrency slots: a free slot is acquired immediately and drop returns
    /// it; when full, an already-cancelled token gives up the queue without
    /// taking a slot. (The static semaphore is process-wide; this is the only
    /// test in the whole suite that touches it, so there is no concurrent
    /// interference)
    #[tokio::test]
    async fn subagent_slot_acquire_and_cancel() {
        let cancel = tokio_util::sync::CancellationToken::new();
        let permit = acquire_subagent_slot(&cancel)
            .await
            .expect("a free slot should be available immediately");
        assert_eq!(subagent_slots_available(), MAX_CONCURRENT_SUBAGENTS - 1);
        drop(permit);
        assert_eq!(subagent_slots_available(), MAX_CONCURRENT_SUBAGENTS);
        // Fill up: acquire with an already-cancelled token immediately returns None
        let mut held = Vec::new();
        for _ in 0..MAX_CONCURRENT_SUBAGENTS {
            held.push(
                acquire_subagent_slot(&cancel)
                    .await
                    .expect("all slots acquirable before filling up"),
            );
        }
        assert_eq!(subagent_slots_available(), 0);
        let cancelled = tokio_util::sync::CancellationToken::new();
        cancelled.cancel();
        assert!(acquire_subagent_slot(&cancelled).await.is_none());
        drop(held);
        assert_eq!(subagent_slots_available(), MAX_CONCURRENT_SUBAGENTS);
    }

    /// `git --exec-path` output → install root: MINGW segment lookup (git
    /// outputs forward-slash paths). Windows version: asserts the native
    /// backslash form (Path equality compares components, so `C:\...` matches
    /// the components of the forward-slash path the function returns).
    #[cfg(windows)]
    #[test]
    fn exec_path_root_inference() {
        let root = root_from_exec_path_text("C:/Program Files/Git/mingw64/libexec/git-core\n")
            .expect("the standard layout should hit");
        assert_eq!(root, PathBuf::from("C:\\Program Files\\Git"));

        let root = root_from_exec_path_text("C:/Git/ucrt64/libexec/git-core").unwrap();
        assert_eq!(root, PathBuf::from("C:\\Git"));

        // Shim layout: an mingw segment at the very front still yields the drive root
        let root = root_from_exec_path_text("D:/mingw64/libexec/git-core").unwrap();
        assert_eq!(root, PathBuf::from("D:\\"));

        // No MINGW segment: fall back two levels up (libexec/git-core → root)
        let root = root_from_exec_path_text("C:/x/libexec/git-core").unwrap();
        assert_eq!(root, PathBuf::from("C:\\x"));

        assert!(root_from_exec_path_text("").is_none());
    }

    /// Unix version: verifies the same logic with native Unix paths (in
    /// production this function is only called on the Windows detection chain;
    /// kept here for logic regression coverage).
    #[cfg(not(windows))]
    #[test]
    fn exec_path_root_inference() {
        let root = root_from_exec_path_text("/opt/Git/mingw64/libexec/git-core\n")
            .expect("the standard layout should hit");
        assert_eq!(root, PathBuf::from("/opt/Git"));

        let root = root_from_exec_path_text("/opt/Git/ucrt64/libexec/git-core").unwrap();
        assert_eq!(root, PathBuf::from("/opt/Git"));

        // Shim layout: an mingw segment right after the root still yields the root
        let root = root_from_exec_path_text("/mingw64/libexec/git-core").unwrap();
        assert_eq!(root, PathBuf::from("/"));

        // No MINGW segment: fall back two levels up (libexec/git-core → root)
        let root = root_from_exec_path_text("/opt/x/libexec/git-core").unwrap();
        assert_eq!(root, PathBuf::from("/opt/x"));

        assert!(root_from_exec_path_text("").is_none());
    }

    /// Infer bash candidates from the git.exe layout: take the grandparent under cmd/bin; other layouts yield no candidates.
    #[test]
    fn git_bash_candidate_inference() {
        let candidates = git_bash_candidates(Path::new("C:/Git/cmd/git.exe"));
        assert_eq!(
            candidates,
            vec![
                PathBuf::from("C:/Git/bin/bash.exe"),
                PathBuf::from("C:/Git/usr/bash.exe")
            ]
        );
        // A shim directory (not cmd/bin) should yield no candidates
        assert!(git_bash_candidates(Path::new("C:/shim/git.exe")).is_empty());
    }

    /// NUL redirect rewrite: only the redirect target is touched, not plain arguments.
    #[test]
    fn nul_redirect_rewrite() {
        assert_eq!(
            rewrite_nul_redirects("ipconfig > NUL 2>&1 && echo ok"),
            "ipconfig > /dev/null 2>&1 && echo ok"
        );
        assert_eq!(rewrite_nul_redirects("dir >>nul"), "dir >>/dev/null");
        assert_eq!(
            rewrite_nul_redirects("echo NUL is a word"),
            "echo NUL is a word"
        );
    }

    /// Unix shell detection: pick the first existing bash candidate path; fall back to sh when none exist.
    #[cfg(not(windows))]
    #[test]
    fn unix_shell_prefers_bash_falls_back_to_sh() {
        let expected = ["/bin/bash", "/usr/bin/bash", "/usr/local/bin/bash"]
            .into_iter()
            .find(|candidate| Path::new(candidate).is_file())
            .unwrap_or("sh");
        assert_eq!(unix_shell(), expected);
    }

    /// When git is installed (a hard pig-code dependency), detection must find
    /// Git Bash. Windows-only: the detection chain looks for .exe/ProgramFiles
    /// layouts, while on Unix spawn goes straight to sh without this chain
    /// (running it here would necessarily fail, which is not a regression
    /// signal).
    #[cfg(windows)]
    #[test]
    fn detection_finds_bash_when_git_present() {
        let git_on_path = std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_ok_and(|out| out.status.success());
        if !git_on_path {
            return; // Skip on machines without git (theoretically nonexistent: pig-code hard-depends on git)
        }
        assert!(
            matches!(windows_shell(), WindowsShell::GitBash(_)),
            "git is available but Git Bash was not detected"
        );
    }
}
