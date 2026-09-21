//! 会话级后台 Bash 任务：注册表保序存放，watcher 收输出、等退出、更新状态，
//! 完成经 task_notify channel 通知 agent_loop 推送 TaskListChanged。不持久化。

use std::path::Path;
use std::sync::{Arc, Mutex};

use pig_protocol::{TaskStatus, TaskSummary};
use tokio::sync::mpsc::UnboundedSender;

use crate::rollout::now_secs;
use crate::tool::TodoHandle;

/// 注册表内 output 滚动上限（追加时从头部截断）。
const MAX_TASK_OUTPUT: usize = 64 * 1024;
/// 面板快照 output_tail 的尾部字符数。
const SNAPSHOT_TAIL_CHARS: usize = 4000;

pub struct TaskEntry {
    pub id: String,
    pub command: String,
    pub status: TaskStatus,
    pub started_at: u64,
    pub ended_at: Option<u64>,
    pub pid: Option<u32>,
    pub output: String,
}

/// 按会话保序的任务注册表（id = b{序号}）。
pub type TaskRegistry = Arc<Mutex<Vec<TaskEntry>>>;

/// 会话级工具共享状态：待办清单 + 后台任务注册表 + 完成通知。
/// 全部字段可 Clone（Arc/sender），Session 与 agent_loop 的 SessionEntry 各持一份共享。
pub struct SessionToolState {
    pub todos: TodoHandle,
    pub tasks: TaskRegistry,
    /// watcher 完成任务后发送 session_id，agent_loop 据此推 TaskListChanged
    pub task_notify: UnboundedSender<String>,
    pub session_id: String,
}

impl Clone for SessionToolState {
    fn clone(&self) -> Self {
        Self {
            todos: self.todos.clone(),
            tasks: self.tasks.clone(),
            task_notify: self.task_notify.clone(),
            session_id: self.session_id.clone(),
        }
    }
}

impl SessionToolState {
    pub fn new(session_id: String, task_notify: UnboundedSender<String>) -> Self {
        Self {
            todos: TodoHandle::default(),
            tasks: TaskRegistry::default(),
            task_notify,
            session_id,
        }
    }

    /// 测试用：session_id = "test"，notify 的 receiver 直接丢弃（send 失败忽略）。
    pub fn for_test() -> Self {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        Self::new("test".to_string(), tx)
    }
}

/// 输出尾部 max 字符（按字符边界截断）。
pub fn tail_chars(text: &str, max: usize) -> String {
    let total = text.chars().count();
    if total <= max {
        return text.to_string();
    }
    text.chars().skip(total - max).collect()
}

/// 面板快照：output_tail 取尾部 4000 字符。
pub fn snapshot(registry: &TaskRegistry) -> Vec<TaskSummary> {
    let tasks = registry.lock().expect("task registry lock");
    tasks
        .iter()
        .map(|entry| TaskSummary {
            id: entry.id.clone(),
            command: entry.command.clone(),
            status: entry.status,
            started_at: entry.started_at,
            ended_at: entry.ended_at,
            output_tail: tail_chars(&entry.output, SNAPSHOT_TAIL_CHARS),
        })
        .collect()
}

fn append_output(registry: &TaskRegistry, task_id: &str, chunk: &[u8]) {
    let mut tasks = registry.lock().expect("task registry lock");
    if let Some(entry) = tasks.iter_mut().find(|t| t.id == task_id) {
        entry.output.push_str(&String::from_utf8_lossy(chunk));
        if entry.output.len() > MAX_TASK_OUTPUT {
            let mut start = entry.output.len() - MAX_TASK_OUTPUT;
            while start < entry.output.len() && !entry.output.is_char_boundary(start) {
                start += 1;
            }
            entry.output.drain(..start);
        }
    }
}

async fn read_into<R: tokio::io::AsyncRead + Unpin>(
    mut reader: R,
    registry: TaskRegistry,
    task_id: String,
) {
    let mut buf = [0u8; 4096];
    loop {
        match tokio::io::AsyncReadExt::read(&mut reader, &mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => append_output(&registry, &task_id, &buf[..n]),
        }
    }
}

/// 后台启动 shell 命令（sh -c / Windows cmd /C，stdout/stderr 收集、stdin 关闭），
/// 注册条目后立即返回 task_id；watcher 并发读两个 pipe → 等退出 → 仅当状态仍是
/// Running 才置 Exited（避免覆写 stop_task 先置的 Killed）→ task_notify 发 session_id。
pub fn spawn_background(state: &SessionToolState, cwd: &Path, command: &str) -> String {
    let mut shell = if cfg!(target_os = "windows") {
        tokio::process::Command::new("cmd")
    } else {
        tokio::process::Command::new("sh")
    };
    shell
        .args(if cfg!(target_os = "windows") {
            vec!["/C", command]
        } else {
            vec!["-c", command]
        })
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let spawned = shell.spawn();
    let task_id = {
        let mut tasks = state.tasks.lock().expect("task registry lock");
        let id = format!("b{}", tasks.len() + 1);
        let (status, ended_at, pid, output) = match &spawned {
            Ok(child) => (TaskStatus::Running, None, child.id(), String::new()),
            Err(e) => (
                TaskStatus::Exited(-1),
                Some(now_secs()),
                None,
                format!("启动失败: {e}"),
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
        });
        id
    };
    let Ok(mut child) = spawned else {
        return task_id;
    };

    let registry = state.tasks.clone();
    let notify = state.task_notify.clone();
    let session_id = state.session_id.clone();
    let watcher_id = task_id.clone();
    tokio::spawn(async move {
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");
        let read_out = read_into(stdout, registry.clone(), watcher_id.clone());
        let read_err = read_into(stderr, registry.clone(), watcher_id.clone());
        let (status, _, _) = tokio::join!(child.wait(), read_out, read_err);
        let code = status.ok().and_then(|s| s.code()).unwrap_or(-1);
        {
            let mut tasks = registry.lock().expect("task registry lock");
            if let Some(entry) = tasks.iter_mut().find(|t| t.id == watcher_id) {
                if matches!(entry.status, TaskStatus::Running) {
                    entry.status = TaskStatus::Exited(code);
                    entry.ended_at = Some(now_secs());
                }
            }
        }
        let _ = notify.send(session_id);
    });
    task_id
}

/// 停止后台任务：找不到 → Err；非 Running → Err「任务已结束」；
/// 先置 Killed + ended_at（防 watcher 覆写状态），再杀直接 pid
///（unix kill -9 / windows taskkill /F /T；不杀进程组，已知取舍），最后 notify。
pub fn stop_task(
    registry: &TaskRegistry,
    task_id: &str,
    notify: &UnboundedSender<String>,
    session_id: &str,
) -> Result<String, String> {
    let pid = {
        let mut tasks = registry.lock().expect("task registry lock");
        let Some(entry) = tasks.iter_mut().find(|t| t.id == task_id) else {
            return Err(format!("任务不存在: {task_id}"));
        };
        if !matches!(entry.status, TaskStatus::Running) {
            return Err(format!("任务已结束: {task_id}"));
        }
        entry.status = TaskStatus::Killed;
        entry.ended_at = Some(now_secs());
        entry.pid
    };
    if let Some(pid) = pid {
        if cfg!(target_os = "windows") {
            let _ = std::process::Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/F", "/T"])
                .output();
        } else {
            let _ = std::process::Command::new("kill")
                .args(["-9", &pid.to_string()])
                .output();
        }
    }
    let _ = notify.send(session_id.to_string());
    Ok(format!("已停止任务 {task_id}"))
}
