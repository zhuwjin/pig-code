use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use pig_protocol::{TaskKind, TaskStatus};
use tokio::sync::mpsc::UnboundedSender;

use crate::rollout::now_secs;

use super::notification::{TaskChange, terminal_change};
use super::output::{OutputMeter, read_into};
use super::registry::{SessionToolState, TaskEntry, TaskRegistry, next_task_id, spill_for};
use super::sandbox::{AdditionalPermissions, SandboxOverride, SandboxProfile};
use super::shell::*;

mod tree;
pub(crate) use tree::{ManagedChild, ProcessTask, spawn};

/// Watcher: wait for child exit + drain both readers → settle a terminal status
/// only when still Running (avoiding overwriting the Killed set earlier by
/// stop_task) → send the session_id via task_notify. Shared by spawn_background
/// and run_foreground's timeout-to-background path.
fn spawn_watcher(
    registry: TaskRegistry,
    notify: UnboundedSender<TaskChange>,
    session_id: String,
    task_id: String,
    mut child: ManagedChild,
    process: ProcessTask,
    read_out: tokio::task::JoinHandle<()>,
    read_err: tokio::task::JoinHandle<()>,
) {
    process.watch();
    tokio::spawn(async move {
        let status = child.wait().await;
        let output_error = [read_out.await, read_err.await]
            .into_iter()
            .find_map(Result::err);
        let quiescence = process.wait_empty().await;
        let code = status.ok().and_then(|s| s.code()).unwrap_or(-1);
        let change = {
            let mut tasks = registry.lock().expect("task registry lock");
            tasks
                .iter_mut()
                .find(|entry| entry.id == task_id)
                .map(|entry| {
                    let mut failures = output_error
                        .map(|error| format!("Task output reader failed: {error}"))
                        .into_iter()
                        .collect::<Vec<_>>();
                    if let Err(error) = &quiescence {
                        process.stop_watching();
                        failures.push(format!(
                            "The command exited, but process-tree quiescence could not be proven: {error}"
                        ));
                    } else {
                        process.mark_quiescent();
                        entry.ended_at.get_or_insert_with(now_secs);
                    }
                    if !failures.is_empty() {
                        entry.reason = Some(failures.join("; "));
                        if matches!(entry.status, TaskStatus::Running) {
                            entry.status = TaskStatus::Failed;
                        }
                        entry.ended_at.get_or_insert_with(now_secs);
                    } else if matches!(entry.status, TaskStatus::Running) {
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
            process: spawned.as_ref().ok().map(ManagedChild::process),
            output,
            spill: spill_for(state, &id),
            cancel: None,
            agent_id: None,
            run: None,
            owner_agent_id: state.owner_agent_id.clone(),
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
    let process = child.process();
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
        process,
        read_out,
        read_err,
    );
    task_id
}

struct ForegroundCancelGuard {
    state: Option<SessionToolState>,
    task_id: String,
    child: Option<ManagedChild>,
    read_out: Option<tokio::task::JoinHandle<()>>,
    read_err: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for ForegroundCancelGuard {
    fn drop(&mut self) {
        let Some(state) = self.state.take() else {
            return;
        };
        let Some(child) = self.child.take() else {
            return;
        };
        let process = child.process();
        let termination = process.terminate().err();
        let cleanup = cleanup_cancelled_foreground(
            state,
            self.task_id.clone(),
            child,
            process,
            self.read_out.take(),
            self.read_err.take(),
            termination,
        );
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(cleanup);
        } else {
            tracing::warn!(
                task_id = self.task_id,
                "foreground command was dropped without an async runtime; artifacts were retained"
            );
        }
    }
}

async fn cleanup_cancelled_foreground(
    state: SessionToolState,
    task_id: String,
    mut child: ManagedChild,
    process: ProcessTask,
    read_out: Option<tokio::task::JoinHandle<()>>,
    read_err: Option<tokio::task::JoinHandle<()>>,
    termination: Option<String>,
) {
    let status = child.wait().await;
    if let Some(read) = read_out
        && let Err(error) = read.await
    {
        tracing::warn!(task_id, %error, "cancelled foreground stdout reader failed");
    }
    if let Some(read) = read_err
        && let Err(error) = read.await
    {
        tracing::warn!(task_id, %error, "cancelled foreground stderr reader failed");
    }
    let quiescence = process.wait_empty().await;
    if let Err(error) = status {
        tracing::warn!(task_id, %error, "cancelled foreground child did not return a status");
    }
    if let Some(error) = termination {
        tracing::warn!(task_id, %error, "cancelled foreground termination request failed");
    }
    let cleanup_error = quiescence.err();
    if cleanup_error.is_none() {
        process.mark_quiescent();
    }
    let spill_path = {
        let tasks = state.tasks.lock().expect("task registry lock");
        tasks
            .iter()
            .find(|task| task.id == task_id && task.foreground)
            .and_then(|entry| entry.spill_path().map(Path::to_path_buf))
    };
    let cleanup_error = if cleanup_error.is_none()
        && let Some(path) = &spill_path
    {
        match std::fs::remove_file(path) {
            Ok(()) => None,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => Some(format!(
                "The process tree is quiescent, but the spill file could not be removed: {error}"
            )),
        }
    } else {
        cleanup_error
    };
    let change = {
        let mut tasks = state.tasks.lock().expect("task registry lock");
        if let Some(error) = cleanup_error {
            if let Some(entry) = tasks.iter_mut().find(|task| task.id == task_id) {
                entry.status = TaskStatus::Failed;
                entry.reason = Some(error);
                entry.ended_at = Some(now_secs());
                entry.foreground = false;
            }
            None
        } else {
            tasks.retain(|task| task.id != task_id);
            Some(TaskChange::Snapshot {
                session_id: state.session_id.clone(),
                task: None,
            })
        }
    };
    let change = change.or_else(|| {
        let tasks = state.tasks.lock().expect("task registry lock");
        tasks
            .iter()
            .find(|task| task.id == task_id)
            .map(|entry| super::notification::snapshot_change(&state.session_id, entry))
    });
    if let Some(change) = change {
        let _ = state.task_notify.send(change);
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
    let child = match spawn_shell_with_profile(
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
            process: Some(child.process()),
            output: String::new(),
            spill: spill_for(state, &id),
            cancel: None,
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
    let mut cancel_guard = ForegroundCancelGuard {
        state: Some(state.clone()),
        task_id: task_id.clone(),
        child: Some(child),
        read_out: None,
        read_err: None,
    };
    let stdout = cancel_guard
        .child
        .as_mut()
        .expect("foreground child")
        .stdout
        .take()
        .expect("stdout piped");
    let stderr = cancel_guard
        .child
        .as_mut()
        .expect("foreground child")
        .stderr
        .take()
        .expect("stderr piped");
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
    cancel_guard.read_out = Some(read_out);
    cancel_guard.read_err = Some(read_err);
    let process = cancel_guard
        .child
        .as_ref()
        .expect("foreground child")
        .process();
    let wait = cancel_guard
        .child
        .as_mut()
        .expect("foreground child")
        .wait();
    match tokio::time::timeout(timeout, wait).await {
        Ok(status) => {
            let mut code = status.ok().and_then(|s| s.code()).unwrap_or(-1);
            // Join the readers first to drain the pipes, then take the full text and remove the registry entry
            let output_error = [
                cancel_guard
                    .read_out
                    .take()
                    .expect("foreground stdout reader")
                    .await,
                cancel_guard
                    .read_err
                    .take()
                    .expect("foreground stderr reader")
                    .await,
            ]
            .into_iter()
            .find_map(Result::err);
            let quiescence = process.wait_empty().await;
            if quiescence.is_ok() {
                process.mark_quiescent();
            }
            let spill_path = {
                let mut tasks = state.tasks.lock().expect("task registry lock");
                let spill_path = tasks
                    .iter()
                    .find(|t| t.id == task_id)
                    .and_then(|entry| entry.spill_path().map(Path::to_path_buf));
                if quiescence.is_ok() {
                    tasks.retain(|t| t.id != task_id);
                } else if let Some(entry) = tasks.iter_mut().find(|t| t.id == task_id) {
                    entry.status = TaskStatus::Failed;
                    entry.reason = quiescence.as_ref().err().cloned();
                    entry.ended_at = Some(now_secs());
                    entry.foreground = false;
                }
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
            if let Some(error) = output_error {
                code = -1;
                output.push_str(&format!("\n\n[Task output reader failed: {error}]"));
            }
            if let Err(error) = quiescence {
                code = -1;
                output.push_str(&format!(
                    "\n\n[The command exited, but process-tree quiescence could not be proven: {error}]"
                ));
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
            let child = cancel_guard.child.take().expect("foreground child");
            let read_out = cancel_guard
                .read_out
                .take()
                .expect("foreground stdout reader");
            let read_err = cancel_guard
                .read_err
                .take()
                .expect("foreground stderr reader");
            spawn_watcher(
                state.tasks.clone(),
                state.task_notify.clone(),
                state.session_id.clone(),
                task_id.clone(),
                child,
                process,
                read_out,
                read_err,
            );
            state.notify_task(&task_id);
            ForegroundOutcome::TimedOut { task_id }
        }
    }
}

pub(crate) struct TaskStopRequest {
    process: Option<ProcessTask>,
    termination_error: Option<String>,
}

pub(crate) fn request_task_stop_with_agents(
    registry: &TaskRegistry,
    task_id: &str,
    notify: &UnboundedSender<TaskChange>,
    session_id: &str,
    agents: Option<&crate::agent::AgentExecutionRegistry>,
) -> Result<TaskStopRequest, String> {
    let run_id = {
        let tasks = registry.lock().expect("task registry lock");
        let Some(entry) = tasks.iter().find(|task| task.id == task_id) else {
            return Err(format!("Task not found: {task_id}"));
        };
        let settled = entry
            .process
            .as_ref()
            .map_or(true, ProcessTask::is_quiescent);
        if !matches!(entry.status, TaskStatus::Running | TaskStatus::Queued) && settled {
            return Err(format!("Task already finished: {task_id}"));
        }
        entry.run.as_ref().map(|run| run.run_id.clone())
    };
    if let (Some(agents), Some(run_id)) = (agents, run_id) {
        agents.request_run_stop(&run_id, "Stopped by user")?;
    }
    let (pid, cancel, process, change) = {
        let mut tasks = registry.lock().expect("task registry lock");
        let Some(entry) = tasks.iter_mut().find(|task| task.id == task_id) else {
            return Err(format!("Task not found: {task_id}"));
        };
        entry.status = TaskStatus::Killed;
        entry.reason = Some("Stop requested by user; waiting for execution to settle".to_string());
        entry.ended_at = None;
        let change = super::notification::snapshot_change(session_id, entry);
        (
            entry.pid,
            entry.cancel.clone(),
            entry.process.clone(),
            change,
        )
    };
    let mut termination_error = None;
    if let Some(token) = cancel {
        token.cancel();
    } else if let Some(process) = &process {
        if let Err(error) = process.terminate() {
            termination_error = Some(error);
        }
    } else if let Some(pid) = pid {
        termination_error = Some(format!(
            "Process {pid} has no managed ProcessTree; quiescence cannot be proven"
        ));
    }
    let _ = notify.send(change);
    Ok(TaskStopRequest {
        process,
        termination_error,
    })
}

pub async fn stop_task(
    registry: &TaskRegistry,
    task_id: &str,
    notify: &UnboundedSender<TaskChange>,
    session_id: &str,
) -> Result<String, String> {
    stop_task_with_agents(registry, task_id, notify, session_id, None).await
}

pub(crate) async fn stop_task_with_agents(
    registry: &TaskRegistry,
    task_id: &str,
    notify: &UnboundedSender<TaskChange>,
    session_id: &str,
    agents: Option<&crate::agent::AgentExecutionRegistry>,
) -> Result<String, String> {
    let request = request_task_stop_with_agents(registry, task_id, notify, session_id, agents)?;
    let Some(process) = request.process else {
        return request.termination_error.map_or_else(
            || Ok(format!("Stop requested for task {task_id}")),
            |error| Err(format!("Failed to stop task {task_id}: {error}")),
        );
    };
    let wait = async {
        process.wait_empty().await?;
        if !process.has_watcher() {
            process.mark_quiescent();
        }
        let mut settled = process.subscribe();
        while !*settled.borrow_and_update() {
            settled
                .changed()
                .await
                .map_err(|_| "the process watcher ended without a quiescence proof".to_string())?;
        }
        Ok::<(), String>(())
    };
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), wait).await;
    let failure = match result {
        Ok(Ok(())) => None,
        Ok(Err(error)) => Some(error),
        Err(_) => Some("timed out after 5 seconds waiting for the process tree".to_string()),
    };
    let Some(mut failure) = failure else {
        if let Some(error) = request.termination_error {
            tracing::warn!(task_id, %error, "termination failed after the process tree was already quiescent");
        }
        return Ok(format!("Stopped task {task_id}"));
    };
    if let Some(error) = request.termination_error {
        failure.push_str(&format!("; termination request failed: {error}"));
    }
    let failure = format!("Task {task_id} is not quiescent: {failure}; retry TaskStop");
    let change = {
        let mut tasks = registry.lock().expect("task registry lock");
        tasks
            .iter_mut()
            .find(|task| task.id == task_id)
            .map(|entry| {
                entry.reason = Some(failure.clone());
                super::notification::snapshot_change(session_id, entry)
            })
    };
    if let Some(change) = change {
        let _ = notify.send(change);
    }
    Err(failure)
}
