//! 会话级 Bash 任务（前台/后台统一注册表）：读者收输出、watcher 等退出更新状态，
//! 完成经 task_notify channel 通知 agent_loop 推送 TaskListChanged。不持久化。
//! 前台超时自动转后台继续跑；spill 文件（.pigcode/tool-results/{id}.log）保存全量输出。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pig_protocol::{TaskStatus, TaskSummary};
use tokio::sync::mpsc::UnboundedSender;

use crate::rollout::now_secs;
use crate::tool::TodoHandle;

/// Read 记录的文件新鲜度状态（ZCode read-file-state 同款）：
/// Write/Edit 写前比对，防止基于过期视图覆盖外部改动。
#[derive(Debug, Clone)]
pub struct ReadState {
    pub mtime: Option<std::time::SystemTime>,
    pub size: u64,
    /// 原始字节的 DefaultHasher 值
    pub hash: u64,
    /// 本次读取是否被预算截断（不完整视图）；显式 offset/limit 分页读不算
    pub partial: bool,
}

/// 注册表内 output 滚动上限（追加时从头部截断）。
const MAX_TASK_OUTPUT: usize = 64 * 1024;
/// 面板快照 output_tail 的尾部字符数。
const SNAPSHOT_TAIL_CHARS: usize = 4000;
/// spill 文件（全量输出落盘）上限 10MB，超出后停止写入。
const MAX_SPILL_BYTES: u64 = 10 * 1024 * 1024;

pub struct TaskEntry {
    pub id: String,
    pub command: String,
    pub status: TaskStatus,
    pub started_at: u64,
    pub ended_at: Option<u64>,
    pub pid: Option<u32>,
    pub output: String,
    /// 全量输出落盘路径（.pigcode/tool-results/{id}.log）
    pub spill_path: Option<PathBuf>,
    /// 子代理后台任务的驱动取消令牌（Bash 恒 None）；stop_task 优先走它而非杀进程树
    pub cancel: Option<tokio_util::sync::CancellationToken>,
    /// 子代理后台任务的 agent_id（Bash 恒 None）；resume 运行中冲突检测用
    pub agent_id: Option<String>,
}

/// 按会话保序的任务注册表（id = b{task_seq 递增}；移除条目不回收序号）。
pub type TaskRegistry = Arc<Mutex<Vec<TaskEntry>>>;

/// 会话级工具共享状态：待办清单 + 任务注册表 + 完成通知 + 唤醒通道 + 任务序号 + 文件新鲜度。
/// 全部字段可 Clone（Arc/atomic/sender），Session 与 agent_loop 的 SessionEntry 各持一份共享。
pub struct SessionToolState {
    pub todos: TodoHandle,
    pub tasks: TaskRegistry,
    /// watcher 完成任务后发送 session_id，agent_loop 据此推 TaskListChanged
    pub task_notify: UnboundedSender<String>,
    /// 后台子代理完成唤醒通道：(session_id, 通知文本)，agent_loop 合成 user 消息起新回合
    pub wake_notify: UnboundedSender<(String, String)>,
    pub session_id: String,
    /// 任务序号发生器（b1、b2…单调递增；前台任务完成移除后不复用）
    pub task_seq: Arc<AtomicUsize>,
    /// Read 登记的文件新鲜度（key = resolve_checked 后的完整路径）
    pub read_states: Arc<Mutex<HashMap<PathBuf, ReadState>>>,
    /// 会话级开关：允许读取工作区外文件（tmp 目录始终放行；敏感文件永远拦截）
    pub fs_read_outside: Arc<AtomicBool>,
    /// 会话级开关：允许写入工作区外文件
    pub fs_write_outside: Arc<AtomicBool>,
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
        }
    }
}

impl SessionToolState {
    pub fn new(
        session_id: String,
        task_notify: UnboundedSender<String>,
        wake_notify: UnboundedSender<(String, String)>,
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
        }
    }

    /// 测试用：session_id = "test"，notify/wake 的 receiver 直接丢弃（send 失败忽略）。
    pub fn for_test() -> Self {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (wake_tx, _wake_rx) = tokio::sync::mpsc::unbounded_channel();
        Self::new("test".to_string(), tx, wake_tx)
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

/// 任务序号：b1、b2…单调递增（前台任务完成会从注册表移除，len+1 有复用风险）
fn next_task_id(seq: &AtomicUsize) -> String {
    format!("b{}", seq.fetch_add(1, Ordering::Relaxed) + 1)
}

/// 注册任务统一分配 spill 路径：{cwd}/.pigcode/tool-results/{task_id}.log
fn spill_path_for(cwd: &Path, task_id: &str) -> PathBuf {
    cwd.join(".pigcode")
        .join("tool-results")
        .join(format!("{task_id}.log"))
}

/// 注册表滚动 output（64KB 头部截断不变）；有 spill_path 同时追加落盘。
fn append_output(registry: &TaskRegistry, task_id: &str, chunk: &[u8]) {
    let spill = {
        let mut tasks = registry.lock().expect("task registry lock");
        let Some(entry) = tasks.iter_mut().find(|t| t.id == task_id) else {
            return;
        };
        entry.output.push_str(&String::from_utf8_lossy(chunk));
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
        append_spill(&path, chunk);
    }
}

/// spill 追加写（每次开闭）：超 10MB 停止写入，截断瞬间只补一次提示。
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
        let _ = file.write_all("[输出超过 10MB，后续已丢弃]".as_bytes());
    }
}

/// 追加文本到 entry.output（沿用 64KB 头部截断；不写 spill）——子代理进度/结果用。
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

/// 注册子代理后台任务条目（Running、无 pid/spill；cancel = 驱动取消令牌，
/// agent_id 供 resume 运行中冲突检测与面板标识），立即 notify 让面板出现条目，返回 task_id。
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
        });
        id
    };
    let _ = state.task_notify.send(state.session_id.clone());
    id
}

/// pipe 读者：chunk → 注册表滚动 output + spill 落盘；sink 非空时另存一份全文
///（前台 Completed 需要 stdout/stderr 分开渲染，注册表那份是合并流）。
async fn read_into<R: tokio::io::AsyncRead + Unpin>(
    mut reader: R,
    registry: TaskRegistry,
    task_id: String,
    sink: Option<Arc<Mutex<String>>>,
) {
    let mut buf = [0u8; 4096];
    loop {
        match tokio::io::AsyncReadExt::read(&mut reader, &mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                append_output(&registry, &task_id, &buf[..n]);
                if let Some(sink) = &sink {
                    sink.lock()
                        .expect("stream sink lock")
                        .push_str(&String::from_utf8_lossy(&buf[..n]));
                }
            }
        }
    }
}

/// 统一起 shell：sh -c / Windows cmd /C、工作目录、stdin null、stdout/stderr piped、
/// kill_on_drop。注入 NO_COLOR=1 / TERM=dumb / GIT_TERMINAL_PROMPT=0
///（防 git 交互提问挂死，kimi-code 同款三件套）。
/// unix 上 process_group(0) 让子进程自成进程组组长，stop_task 才能整组树杀。
fn spawn_shell(cwd: &Path, command: &str) -> std::io::Result<tokio::process::Child> {
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
        .env("NO_COLOR", "1")
        .env("TERM", "dumb")
        .env("GIT_TERMINAL_PROMPT", "0")
        .kill_on_drop(true);
    #[cfg(unix)]
    shell.process_group(0);
    shell.spawn()
}

/// watcher：等子进程退出 + 排空两个读者 → 仅当状态仍是 Running 才置 Exited
///（避免覆写 stop_task 先置的 Killed）→ task_notify 发 session_id。
/// spawn_background 与 run_foreground 超时转后台共用。
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
            if let Some(entry) = tasks.iter_mut().find(|t| t.id == task_id) {
                if matches!(entry.status, TaskStatus::Running) {
                    entry.status = TaskStatus::Exited(code);
                    entry.ended_at = Some(now_secs());
                }
            }
        }
        let _ = notify.send(session_id);
    });
}

/// 后台启动 shell 命令：注册条目（Running、pid、spill 路径）后立即返回 task_id；
/// 读者并发收 stdout/stderr（合并进注册表滚动 output + spill 落盘），watcher 等退出收尾。
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
            spill_path: Some(spill_path_for(cwd, &id)),
            cancel: None,
            agent_id: None,
        });
        id
    };
    let Ok(mut child) = spawned else {
        return task_id;
    };
    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");
    let read_out = tokio::spawn(read_into(
        stdout,
        state.tasks.clone(),
        task_id.clone(),
        None,
    ));
    let read_err = tokio::spawn(read_into(
        stderr,
        state.tasks.clone(),
        task_id.clone(),
        None,
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

/// 前台执行结果。
pub enum ForegroundOutcome {
    /// 完成：output 已按「stdout + [stderr] 段」拼装；前台任务已从注册表移除（不留痕）。
    /// 注册表 output 有 64KB 滚动上限，spill 文件才是全量（≤10MB）。
    Completed {
        output: String,
        code: i32,
        spill_path: Option<PathBuf>,
    },
    /// 超时：命令留在注册表继续跑（watcher 已接管），task_id 可查可停
    TimedOut {
        task_id: String,
    },
    SpawnFailed {
        error: String,
    },
}

/// 前台跑 shell 命令：注册 Running 条目 → 读者分流 stdout/stderr → 限时等退出。
/// 完成则排空管道取全文（注册表移除不留痕）；超时则 watcher 接管、转后台继续跑。
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
        });
        id
    };
    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");
    let stdout_sink = Arc::new(Mutex::new(String::new()));
    let stderr_sink = Arc::new(Mutex::new(String::new()));
    let read_out = tokio::spawn(read_into(
        stdout,
        state.tasks.clone(),
        task_id.clone(),
        Some(stdout_sink.clone()),
    ));
    let read_err = tokio::spawn(read_into(
        stderr,
        state.tasks.clone(),
        task_id.clone(),
        Some(stderr_sink.clone()),
    ));
    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(status) => {
            let code = status.ok().and_then(|s| s.code()).unwrap_or(-1);
            // 先 join 读者排空管道，再取全文、移除注册表条目
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
            ForegroundOutcome::Completed {
                output,
                code,
                spill_path,
            }
        }
        Err(_) => {
            // 超时转后台：watcher 接管 child 与读者（sink 随读者存活至进程退出）
            spawn_watcher(
                state.tasks.clone(),
                state.task_notify.clone(),
                state.session_id.clone(),
                task_id.clone(),
                child,
                read_out,
                read_err,
            );
            ForegroundOutcome::TimedOut { task_id }
        }
    }
}

/// 停止任务：找不到 → Err；非 Running → Err「任务已结束」；先置 Killed + ended_at
///（防 watcher/驱动覆写状态）。子代理任务（cancel 令牌在）走令牌取消——驱动循环
/// 自己收尾，无进程可杀；Bash 任务树杀：unix 先 kill -9 整个进程组（spawn_shell 里
/// process_group(0) 使子进程自成组长），再 kill -9 直接 pid 兜底（组已散时无妨）；
/// windows taskkill /F /T。最后 notify。
pub fn stop_task(
    registry: &TaskRegistry,
    task_id: &str,
    notify: &UnboundedSender<String>,
    session_id: &str,
) -> Result<String, String> {
    let (pid, cancel) = {
        let mut tasks = registry.lock().expect("task registry lock");
        let Some(entry) = tasks.iter_mut().find(|t| t.id == task_id) else {
            return Err(format!("任务不存在: {task_id}"));
        };
        if !matches!(entry.status, TaskStatus::Running) {
            return Err(format!("任务已结束: {task_id}"));
        }
        entry.status = TaskStatus::Killed;
        entry.ended_at = Some(now_secs());
        (entry.pid, entry.cancel.clone())
    };
    if let Some(token) = cancel {
        token.cancel();
    } else if let Some(pid) = pid {
        if cfg!(target_os = "windows") {
            let _ = std::process::Command::new("taskkill")
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
    let _ = notify.send(session_id.to_string());
    Ok(format!("已停止任务 {task_id}"))
}
