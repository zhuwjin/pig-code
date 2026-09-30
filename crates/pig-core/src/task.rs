//! 会话级 Bash 任务（前台/后台统一注册表）：读者收输出、watcher 等退出更新状态，
//! 完成经 task_notify channel 通知 agent_loop 推送 TaskListChanged。不持久化。
//! 前台超时自动转后台继续跑；spill 文件（.pigcode/tool-results/{id}.log）保存全量输出。
//! 前台执行中的条目对快照隐藏（普通命令不是后台任务），超时转后台时翻转 foreground 才上屏。

use crate::NoConsoleExt as _;
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
    /// 最近一次成功 Read 的视图参数（offset, limit, column_offset）：
    /// 同参数重读且 hash 未变时短路返回「文件未变化」省 token。
    /// Write/Edit/check_fresh 的内部刷新不带视图（None）。
    pub view: Option<(usize, usize, usize)>,
}

/// 注册表内 output 滚动上限（追加时从头部截断）。
const MAX_TASK_OUTPUT: usize = 64 * 1024;
/// 面板快照 output_tail 的尾部字符数。
const SNAPSHOT_TAIL_CHARS: usize = 4000;
/// spill 文件（全量输出落盘）上限 10MB，超出后停止写入。
const MAX_SPILL_BYTES: u64 = 10 * 1024 * 1024;
/// 任务累计输出上限（stdout+stderr 合计）：超过即强制停止任务——
/// 防「狂喷输出的命令」在超时窗口内吃满前台 sink 内存，或转后台后无限跑下去
///（kimi-code 同款 16MiB 强杀；spill 的 10MB 落盘上限与此独立）。
const MAX_TASK_OUTPUT_TOTAL: usize = 16 * 1024 * 1024;

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
    /// 前台执行中的普通命令：snapshot 对面板隐藏（它不是后台任务）；
    /// 仅在超时转后台时翻转为 false，从此对快照可见
    pub foreground: bool,
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
    /// 始终可读的额外根（构造时尽量 canonicalize）：data_dir/sessions 子树
    ///（子代理 result.md/上下文 jsonl），Read 豁免与 tmp 并列；只放读不放写
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
            // 尽量 canonicalize（目录不存在则用原路径）：白名单比对在 canonical 口径下进行
            extra_read_roots: extra_read_roots
                .into_iter()
                .map(|root| root.canonicalize().unwrap_or(root))
                .collect(),
        }
    }

    /// 测试用：session_id = "test"，notify/wake 的 receiver 直接丢弃（send 失败忽略）。
    /// extra_read_roots 给 tmp 目录（tmp 本就豁免，不改变既有用例行为）。
    pub fn for_test() -> Self {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (wake_tx, _wake_rx) = tokio::sync::mpsc::unbounded_channel();
        Self::new("test".to_string(), tx, wake_tx, vec![std::env::temp_dir()])
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

/// 面板快照：output_tail 取尾部 4000 字符；前台执行中的条目不上屏
///（普通命令不是后台任务，避免执行期间的任意 notify 把它推成「后台 Bash · 运行中」）。
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
/// raw 进 spill（字节保真，含挂起中的不完整序列）；text 是解码后的视图
///（StreamDecoder 产出，Windows 上对非 UTF-8 输出按 GBK 回退）。
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
            foreground: false,
        });
        id
    };
    let _ = state.task_notify.send(state.session_id.clone());
    id
}

/// 子代理全局并发上限（后台 Agent 与 AgentSwarm 展开的子代理共享的信号量槽数）。
/// 配置项需要动 pig-protocol 的 AppConfig（跨 crate），v1 先常量。
pub const MAX_CONCURRENT_SUBAGENTS: usize = 8;

/// 子代理全局并发槽：进程级静态（跨会话共享），超限在 acquire 处排队。
static SUBAGENT_SLOTS: tokio::sync::Semaphore =
    tokio::sync::Semaphore::const_new(MAX_CONCURRENT_SUBAGENTS);

/// 当前空闲并发槽数（排队提示用；瞬时值，与随后的 acquire 结果可能有出入）
pub fn subagent_slots_available() -> usize {
    SUBAGENT_SLOTS.available_permits()
}

/// 申领一个子代理并发槽：超限排队；等待中 cancel 触发返回 None（排队即放弃，不占槽）。
/// permit 是 RAII 守卫：drop 即还槽。
pub async fn acquire_subagent_slot(
    cancel: &tokio_util::sync::CancellationToken,
) -> Option<tokio::sync::SemaphorePermit<'static>> {
    tokio::select! {
        permit = SUBAGENT_SLOTS.acquire() => permit.ok(),
        _ = cancel.cancelled() => None,
    }
}

/// 注册「排队中」的子代理任务：与 register_agent_task 同规格，command 带
/// 「排队中 · 」前缀（TaskStatus 无 Pending 变体、协议不动——TaskList/面板经
/// command 文本可见排队态）；并发槽到手后 mark_agent_task_started 摘前缀。
pub fn register_agent_task_queued(
    state: &SessionToolState,
    command: String,
    cancel: tokio_util::sync::CancellationToken,
    agent_id: String,
) -> String {
    register_agent_task(state, format!("排队中 · {command}"), cancel, agent_id)
}

/// 并发槽到手：摘掉「排队中 · 」前缀并 notify 刷新面板（started_at 不动，
/// 耗时含排队——排队多久本来就是等待成本）。
pub fn mark_agent_task_started(state: &SessionToolState, task_id: &str) {
    {
        let mut tasks = state.tasks.lock().expect("task registry lock");
        if let Some(entry) = tasks.iter_mut().find(|t| t.id == task_id)
            && let Some(rest) = entry.command.strip_prefix("排队中 · ")
        {
            entry.command = rest.to_string();
        }
    }
    let _ = state.task_notify.send(state.session_id.clone());
}

/// 任务输出累计计量（stdout/stderr 两个读者共享）：越过总量上限只触发一次强停。
#[derive(Default)]
struct OutputMeter {
    total: AtomicUsize,
    capped: AtomicBool,
}

impl OutputMeter {
    /// 累计 n 字节；返回 true 表示「本次越过上限且此前未标记」，调用方执行一次性强停。
    fn add(&self, n: usize) -> bool {
        let prev = self.total.fetch_add(n, Ordering::Relaxed);
        prev + n > MAX_TASK_OUTPUT_TOTAL && !self.capped.swap(true, Ordering::Relaxed)
    }
}

/// 输出超限的一次性强停：置 Killed（watcher 只在仍为 Running 时才写 Exited，
/// 不会覆写）、output 落提示（滚动窗口保尾，提示恰好留在末尾）、杀进程树。
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
            .push_str("\n[输出超过 16MiB 上限，任务已被强制停止。请把大输出重定向到文件（如 command > out.txt）后用 Read/Grep 处理]");
        entry.pid
    };
    if let Some(pid) = pid {
        kill_process_tree(pid);
    }
}

/// 杀进程树：Windows taskkill /T（含子进程）；unix 先杀进程组再补杀 pid
///（spawn_shell 里 process_group(0) 使子进程自成组长）。
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

/// pipe 读者：chunk → 解码（StreamDecoder：UTF-8 优先/Windows GBK 回退/跨块挂起）
/// → 注册表滚动 output + spill 落盘（原始字节）；sink 非空时另存一份全文
///（前台 Completed 需要 stdout/stderr 分开渲染，注册表那份是合并流）。
/// 越过 16MiB 总量上限时强停任务并停止读取。
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
    // 流结束：残留的不完整尾部字节出清（不进 spill——原始字节已按块落过盘）
    let tail = decoder.finish();
    if !tail.is_empty() {
        append_output(&registry, &task_id, b"", &tail);
    }
}

mod shell;

pub(crate) use shell::*;
pub use shell::{WindowsShell, shell_label, windows_shell};

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
            foreground: false,
        });
        id
    };
    // 注册即 notify：chip/面板立刻上屏（启动失败同样推，让失败条目可见）
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

/// run_foreground 的取消收尾：执行中 future 被 drop（用户点停止、回合中止）时，
/// 进程随 kill_on_drop 已杀，注册表条目与 spill 文件移除不留痕，notify 刷新面板。
/// 正常完成/超时转后台路径在返回前 disarm（state 置 None），drop 时不再动作。
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
            // 只收尾仍是前台状态的条目（已转后台/已移除的不归它管）
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
/// 完成则排空管道取全文（注册表移除不留痕）；超时则 watcher 接管、转后台继续跑；
/// 执行中被取消（future drop）由 guard 收尾移除条目，不残留「运行中」。
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
            // 正常完成：guard 不再负责收尾
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
            // 输出总量超限被强停：完成的输出尾部补说明（注册表那条提示
            // 随条目一起移除了，模型只能看到这里的 sink 全文）
            if meter.capped.load(Ordering::Relaxed) {
                output.push_str(
                    "\n\n[输出超过 16MiB 上限，命令已被强制停止。请把大输出重定向到文件（如 command > out.txt）后用 Read/Grep 处理]",
                );
            }
            ForegroundOutcome::Completed {
                output,
                code,
                spill_path,
            }
        }
        Err(_) => {
            // 超时转后台：条目从「前台隐藏」转为正式后台任务（快照可见），
            // 立即 notify 上屏；watcher 接管 child 与读者（sink 随读者存活至进程退出）
            {
                let mut tasks = state.tasks.lock().expect("task registry lock");
                if let Some(entry) = tasks.iter_mut().find(|t| t.id == task_id) {
                    entry.foreground = false;
                }
            }
            // 已转正为后台任务：guard 不再负责收尾
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
        kill_process_tree(pid);
    }
    let _ = notify.send(session_id.to_string());
    Ok(format!("已停止任务 {task_id}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 排队注册：command 带前缀；槽到手摘前缀；未知 task_id 不 panic
    #[test]
    fn queued_registration_marks_pending() {
        let state = SessionToolState::for_test();
        let task_id = register_agent_task_queued(
            &state,
            "子代理 explore: 查问题".to_string(),
            tokio_util::sync::CancellationToken::new(),
            "a1-1".to_string(),
        );
        {
            let tasks = state.tasks.lock().expect("task registry lock");
            let entry = tasks.iter().find(|t| t.id == task_id).expect("条目存在");
            assert!(
                entry.command.starts_with("排队中 · "),
                "排队态经 command 前缀呈现: {}",
                entry.command
            );
            assert!(matches!(entry.status, TaskStatus::Running));
        }
        mark_agent_task_started(&state, &task_id);
        {
            let tasks = state.tasks.lock().expect("task registry lock");
            let entry = tasks.iter().find(|t| t.id == task_id).expect("条目存在");
            assert_eq!(entry.command, "子代理 explore: 查问题", "开始后摘前缀");
        }
        mark_agent_task_started(&state, "b999");
    }

    /// 并发槽：空槽立即可得、drop 还槽；占满时已取消的 token 排队即放弃不占槽。
    ///（静态信号量全进程共享，整个测试套件只有这一个用例碰它，无并发干扰）
    #[tokio::test]
    async fn subagent_slot_acquire_and_cancel() {
        let cancel = tokio_util::sync::CancellationToken::new();
        let permit = acquire_subagent_slot(&cancel)
            .await
            .expect("空槽应立即可得");
        assert_eq!(subagent_slots_available(), MAX_CONCURRENT_SUBAGENTS - 1);
        drop(permit);
        assert_eq!(subagent_slots_available(), MAX_CONCURRENT_SUBAGENTS);
        // 占满：已取消 token 的 acquire 立即返回 None
        let mut held = Vec::new();
        for _ in 0..MAX_CONCURRENT_SUBAGENTS {
            held.push(
                acquire_subagent_slot(&cancel)
                    .await
                    .expect("占满前都应可得"),
            );
        }
        assert_eq!(subagent_slots_available(), 0);
        let cancelled = tokio_util::sync::CancellationToken::new();
        cancelled.cancel();
        assert!(acquire_subagent_slot(&cancelled).await.is_none());
        drop(held);
        assert_eq!(subagent_slots_available(), MAX_CONCURRENT_SUBAGENTS);
    }

    /// `git --exec-path` 输出 → 安装根：MINGW 段定位（git 输出正斜杠路径）。
    #[test]
    fn exec_path_root_inference() {
        let root = root_from_exec_path_text("C:/Program Files/Git/mingw64/libexec/git-core\n")
            .expect("常规布局应命中");
        assert_eq!(root, PathBuf::from("C:\\Program Files\\Git"));

        let root = root_from_exec_path_text("C:/Git/ucrt64/libexec/git-core").unwrap();
        assert_eq!(root, PathBuf::from("C:\\Git"));

        // shim 布局：mingw 段在最前也能取到盘符根
        let root = root_from_exec_path_text("D:/mingw64/libexec/git-core").unwrap();
        assert_eq!(root, PathBuf::from("D:\\"));

        // 无 MINGW 段：往上两级兜底（libexec/git-core → 根）
        let root = root_from_exec_path_text("C:/x/libexec/git-core").unwrap();
        assert_eq!(root, PathBuf::from("C:\\x"));

        assert!(root_from_exec_path_text("").is_none());
    }

    /// git.exe 布局反推 bash 候选：cmd/bin 下取上上级；其他布局不出候选。
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
        // shim 目录（非 cmd/bin）不该出候选
        assert!(git_bash_candidates(Path::new("C:/shim/git.exe")).is_empty());
    }

    /// NUL 重定向改写：只动重定向目标，不动普通参数。
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

    /// 本机有 git 时（pig-code 硬依赖），探测必须找到 Git Bash。
    #[test]
    fn detection_finds_bash_when_git_present() {
        let git_on_path = std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_ok_and(|out| out.status.success());
        if !git_on_path {
            return; // 无 git 的机器（理论不存在：pig-code 硬依赖 git）跳过
        }
        assert!(
            matches!(windows_shell(), WindowsShell::GitBash(_)),
            "git 可用却没探测到 Git Bash"
        );
    }
}
