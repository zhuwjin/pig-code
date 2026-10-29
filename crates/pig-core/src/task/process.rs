use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use pig_protocol::{TaskKind, TaskStatus};
use tokio::sync::mpsc::UnboundedSender;

use crate::rollout::now_secs;

use super::notification::{TaskChange, terminal_change};
use super::output::{OutputMeter, kill_process_tree, read_into};
use super::registry::{SessionToolState, TaskEntry, TaskRegistry, next_task_id, spill_for};
use super::sandbox::{AdditionalPermissions, SandboxOverride, SandboxProfile};
use super::shell::*;

/// Watcher: wait for child exit + drain both readers → settle a terminal status
/// only when still Running (avoiding overwriting the Killed set earlier by
/// stop_task) → send the session_id via task_notify. Shared by spawn_background
/// and run_foreground's timeout-to-background path.
fn spawn_watcher(
    registry: TaskRegistry,
    notify: UnboundedSender<TaskChange>,
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
        let change = {
            let mut tasks = registry.lock().expect("task registry lock");
            tasks
                .iter_mut()
                .find(|entry| entry.id == task_id)
                .map(|entry| {
                    if matches!(entry.status, TaskStatus::Running) {
                        entry.status = if code == 0 {
                            TaskStatus::Completed
                        } else {
                            TaskStatus::Failed
                        };
                        entry.exit_code = Some(code);
                        entry.ended_at = Some(now_secs());
                    }
                    terminal_change(&session_id, entry)
                })
        };
        if let Some(change) = change {
            let _ = notify.send(change);
        }
    });
}

/// Start a shell command in the background: register the entry (Running, pid,
/// spill path) and return the task_id immediately; readers concurrently collect
/// stdout/stderr (merged into the registry rolling output + spill persist), and
/// the watcher waits for exit to finish up.
pub fn spawn_background(state: &SessionToolState, cwd: &Path, command: &str) -> String {
    let shell = default_model_shell();
    spawn_background_with_profile(
        state,
        cwd,
        cwd,
        command,
        &shell,
        SandboxOverride::NoOverride,
        SandboxProfile::WorkspaceWrite,
        &AdditionalPermissions::default(),
    )
}

/// `workspace` is the sandbox policy root and the spill-file base; `workdir` is
/// the process working directory for this command only.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_background_with_profile(
    state: &SessionToolState,
    workspace: &Path,
    workdir: &Path,
    command: &str,
    shell: &ModelShell,
    sandbox_override: SandboxOverride,
    profile: SandboxProfile,
    additional: &AdditionalPermissions,
) -> String {
    let spawned = spawn_shell_with_profile(
        state,
        workspace,
        workdir,
        command,
        shell,
        sandbox_override,
        profile,
        additional,
    );
    let task_id = {
        let mut tasks = state.tasks.lock().expect("task registry lock");
        let id = next_task_id(&state.task_seq);
        let (status, ended_at, pid, output, reason) = match &spawned {
            Ok(child) => (TaskStatus::Running, None, child.id(), String::new(), None),
            Err(error) => (
                TaskStatus::Failed,
                Some(now_secs()),
                None,
                format!("Failed to start: {error}"),
                Some(error.to_string()),
            ),
        };
        let exit_code = spawned.as_ref().err().map(|_| -1);
        tasks.push(TaskEntry {
            id: id.clone(),
            kind: TaskKind::Shell,
            command: command.to_string(),
            status,
            exit_code,
            reason,
            started_at: now_secs(),
            ended_at,
            pid,
            output,
            spill: spill_for(state, &id),
            cancel: None,
            agent_id: None,
            record_path: None,
            result_path: None,
            turns: None,
            model: None,
            terminal_notification_suppressed: false,
            foreground: false,
        });
        id
    };
    if spawned.is_err() {
        state.notify_terminal(&task_id);
    } else {
        state.notify_task(&task_id);
    }
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
        let spill_path = {
            let mut tasks = state.tasks.lock().expect("task registry lock");
            // Only clean up entries still in the foreground state (already backgrounded/removed ones are not its business)
            tasks
                .iter()
                .position(|t| t.id == self.task_id && t.foreground)
                .and_then(|ix| tasks.remove(ix).spill_path().map(Path::to_path_buf))
        };
        if let Some(spill_path) = spill_path {
            let _ = std::fs::remove_file(spill_path);
        }
        let _ = state.task_notify.send(TaskChange::Snapshot {
            session_id: state.session_id.clone(),
            task: None,
        });
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
    run_foreground_with_override(state, cwd, command, timeout, SandboxOverride::NoOverride).await
}

pub(crate) async fn run_foreground_with_override(
    state: &SessionToolState,
    cwd: &Path,
    command: &str,
    timeout: std::time::Duration,
    sandbox_override: SandboxOverride,
) -> ForegroundOutcome {
    let shell = default_model_shell();
    run_foreground_with_profile(
        state,
        cwd,
        cwd,
        command,
        &shell,
        timeout,
        sandbox_override,
        SandboxProfile::WorkspaceWrite,
        &AdditionalPermissions::default(),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_foreground_with_profile(
    state: &SessionToolState,
    workspace: &Path,
    workdir: &Path,
    command: &str,
    shell: &ModelShell,
    timeout: std::time::Duration,
    sandbox_override: SandboxOverride,
    profile: SandboxProfile,
    additional: &AdditionalPermissions,
) -> ForegroundOutcome {
    let mut child = match spawn_shell_with_profile(
        state,
        workspace,
        workdir,
        command,
        shell,
        sandbox_override,
        profile,
        additional,
    ) {
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
            kind: TaskKind::Shell,
            command: command.to_string(),
            status: TaskStatus::Running,
            exit_code: None,
            reason: None,
            started_at: now_secs(),
            ended_at: None,
            pid: child.id(),
            output: String::new(),
            spill: spill_for(state, &id),
            cancel: None,
            agent_id: None,
            record_path: None,
            result_path: None,
            turns: None,
            model: None,
            terminal_notification_suppressed: false,
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
                    .and_then(|entry| entry.spill_path().map(Path::to_path_buf));
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
            state.notify_task(&task_id);
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
    notify: &UnboundedSender<TaskChange>,
    session_id: &str,
) -> Result<String, String> {
    let (pid, cancel, change) = {
        let mut tasks = registry.lock().expect("task registry lock");
        let Some(entry) = tasks.iter_mut().find(|t| t.id == task_id) else {
            return Err(format!("Task not found: {task_id}"));
        };
        if !matches!(entry.status, TaskStatus::Running | TaskStatus::Queued) {
            return Err(format!("Task already finished: {task_id}"));
        }
        entry.status = TaskStatus::Killed;
        entry.reason = Some("Stopped by user".to_string());
        entry.ended_at = Some(now_secs());
        let change = super::notification::snapshot_change(session_id, entry);
        (entry.pid, entry.cancel.clone(), change)
    };
    if let Some(token) = cancel {
        token.cancel();
    } else if let Some(pid) = pid {
        kill_process_tree(pid);
    }
    let _ = notify.send(change);
    Ok(format!("Stopped task {task_id}"))
}
