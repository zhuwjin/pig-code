use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pig_protocol::{TaskKind, TaskStatus, TaskSummary};
use tokio::sync::mpsc::UnboundedSender;

use crate::rollout::now_secs;
use crate::store::BackgroundTask;
use crate::tool::TodoHandle;

use super::spill;

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

pub struct TaskEntry {
    pub id: String,
    pub kind: TaskKind,
    pub command: String,
    pub status: TaskStatus,
    pub exit_code: Option<i32>,
    pub reason: Option<String>,
    pub started_at: u64,
    pub ended_at: Option<u64>,
    pub pid: Option<u32>,
    pub output: String,
    /// Full-output spill (lazily created under the session's data-dir tool-results dir)
    pub(super) spill: Option<spill::Spill>,
    /// Driver cancellation token for subagent tasks (always None for shell commands); stop_task prefers it over killing the process tree
    pub cancel: Option<tokio_util::sync::CancellationToken>,
    /// agent_id of subagent tasks (always None for shell commands); used for resume-while-running conflict detection
    pub agent_id: Option<String>,
    pub record_path: Option<PathBuf>,
    pub result_path: Option<PathBuf>,
    pub turns: Option<u32>,
    pub model: Option<String>,
    pub terminal_notification_suppressed: bool,
    /// A plain command running in the foreground: hidden from panel snapshots
    /// (it is not a background task); flipped to false only when a timeout
    /// converts it to a background task, after which it is visible to snapshots
    pub foreground: bool,
}

impl TaskEntry {
    /// Spill path reported to the panel even before the file exists (the panel
    /// falls back to `output_tail` until the lazy file appears).
    pub(super) fn spill_path(&self) -> Option<&Path> {
        self.spill.as_ref().map(spill::Spill::path)
    }

    pub fn summary(&self) -> TaskSummary {
        TaskSummary {
            id: self.id.clone(),
            kind: self.kind,
            command: self.command.clone(),
            status: self.status,
            exit_code: self.exit_code,
            reason: self.reason.clone(),
            started_at_ms: self.started_at.saturating_mul(1_000),
            ended_at_ms: self.ended_at.map(|value| value.saturating_mul(1_000)),
            output_tail: tail_chars(&self.output, SNAPSHOT_TAIL_CHARS),
            agent_id: self.agent_id.clone(),
            spill_path: self.spill_path().map(Path::to_path_buf),
        }
    }

    pub fn background(&self) -> BackgroundTask {
        BackgroundTask {
            summary: self.summary(),
            detached: !self.foreground,
            record_path: self.record_path.clone(),
            result_path: self.result_path.clone(),
            turns: self.turns,
            model: self.model.clone(),
            terminal_notification_suppressed: self.terminal_notification_suppressed,
        }
    }
}

#[derive(Clone)]
pub struct TaskRegistry {
    entries: Arc<Mutex<Vec<TaskEntry>>>,
    revision: tokio::sync::watch::Sender<u64>,
}

impl Default for TaskRegistry {
    fn default() -> Self {
        let (revision, _) = tokio::sync::watch::channel(0);
        Self {
            entries: Arc::new(Mutex::new(Vec::new())),
            revision,
        }
    }
}

impl TaskRegistry {
    pub fn lock(&self) -> Result<RegistryGuard<'_>, std::sync::PoisonError<RegistryGuard<'_>>> {
        self.entries
            .lock()
            .map(|guard| RegistryGuard {
                guard,
                revision: &self.revision,
                changed: false,
            })
            .map_err(|poisoned| {
                std::sync::PoisonError::new(RegistryGuard {
                    guard: poisoned.into_inner(),
                    revision: &self.revision,
                    changed: false,
                })
            })
    }

    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.revision.subscribe()
    }
}

pub struct RegistryGuard<'a> {
    guard: std::sync::MutexGuard<'a, Vec<TaskEntry>>,
    revision: &'a tokio::sync::watch::Sender<u64>,
    changed: bool,
}

impl Drop for RegistryGuard<'_> {
    fn drop(&mut self) {
        if self.changed {
            self.revision.send_modify(|revision| *revision += 1);
        }
    }
}

impl std::ops::Deref for RegistryGuard<'_> {
    type Target = Vec<TaskEntry>;

    fn deref(&self) -> &Self::Target {
        &self.guard
    }
}

impl std::ops::DerefMut for RegistryGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.changed = true;
        &mut self.guard
    }
}

/// Exclusive ownership of one subagent context JSONL for the lifetime of its
/// driver. The guard is released on drop, so normal completion, cancellation
/// and panic all clear the claim before another resume can append.
#[derive(Debug)]
pub struct AgentWriterGuard {
    registry: Arc<Mutex<HashSet<String>>>,
    agent_id: String,
    released: bool,
}

impl AgentWriterGuard {
    /// Release the claim early (terminal cleanup); dropping the guard also
    /// releases it, so this is idempotent.
    pub fn release(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        self.registry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.agent_id);
    }
}

impl Drop for AgentWriterGuard {
    fn drop(&mut self) {
        self.release();
    }
}

/// Session-scoped shared tool state: todo list + task registry + TaskChange
/// channel + task sequence + file freshness. All fields are
/// Clone-able (Arc/atomic/sender); Session and agent_loop's SessionEntry each
/// hold one shared copy.
pub struct SessionToolState {
    pub todos: TodoHandle,
    pub tasks: TaskRegistry,
    /// Task snapshots and terminal notifications share one channel to the manager.
    pub task_notify: UnboundedSender<super::notification::TaskChange>,
    pub session_id: String,
    /// Only the root session may acknowledge delivery into its own rollout.
    pub task_store: Option<Arc<Mutex<crate::store::Store>>>,
    pub delivery_receipts: Arc<Mutex<HashMap<String, super::notification::DeliveryReceipt>>>,
    /// Session data directory: task spills live under
    /// `{data_dir}/sessions/{session_id}/tool-results`. `None` disables spills
    /// (bare test states); sessions attach it during assembly.
    data_dir: Option<PathBuf>,
    /// Task sequence generator (b1, b2... monotonically increasing; not reused after foreground tasks are removed on completion)
    pub task_seq: Arc<AtomicUsize>,
    /// File freshness registered by Read (key = full path after resolve_checked)
    pub read_states: Arc<Mutex<HashMap<PathBuf, ReadState>>>,
    /// Extra always-readable roots (canonicalized when possible at
    /// construction): the data_dir/sessions subtree (subagent result.md/context
    /// jsonl), exempted for Read alongside tmp; read-only, never writable
    pub extra_read_roots: Vec<PathBuf>,
    /// Extra always-readable-AND-writable roots (canonicalized when possible
    /// at construction): the workspace memory directory when memory is on —
    /// the model manages its memory files with the plain Write/Edit tools,
    /// approval-free in both directions (pig's own data, never user
    /// workspace files); effective only for absolute-path requests, same as
    /// the read-only roots
    pub extra_rw_roots: Vec<PathBuf>,
    /// Subagent steer registry: agent_id → queued coordinator messages. A
    /// drive loop registers its key at start (presence = running) and drains
    /// the queue at its step boundaries; the SendMessage tool pushes here
    /// (shared Arc — tools and drives both reach it through the state)
    pub agent_steers: Arc<Mutex<HashMap<String, std::collections::VecDeque<String>>>>,
    /// Exclusive subagent JSONL writers: agent_id → claimed by one live
    /// driver. Claimed before any context write and released on every exit
    /// path (including panic), so a resume cannot open a second writer while
    /// a stopped driver is still closing.
    pub agent_writers: Arc<Mutex<HashSet<String>>>,
    /// The workspace's own sandbox layer (`<workspace>/.pigcode/config.toml`),
    /// applied over the user-level settings at launch time. Shared rather than
    /// copied, so a trust decision made mid-session reaches every clone of the
    /// state (the tool path holds its own).
    pub sandbox_project: Arc<std::sync::RwLock<crate::config::ProjectSandboxLayer>>,
}

impl Clone for SessionToolState {
    fn clone(&self) -> Self {
        Self {
            todos: self.todos.clone(),
            tasks: self.tasks.clone(),
            task_notify: self.task_notify.clone(),
            session_id: self.session_id.clone(),
            task_store: self.task_store.clone(),
            delivery_receipts: self.delivery_receipts.clone(),
            data_dir: self.data_dir.clone(),
            task_seq: self.task_seq.clone(),
            read_states: self.read_states.clone(),
            agent_steers: self.agent_steers.clone(),
            agent_writers: self.agent_writers.clone(),
            extra_read_roots: self.extra_read_roots.clone(),
            extra_rw_roots: self.extra_rw_roots.clone(),
            sandbox_project: self.sandbox_project.clone(),
        }
    }
}

impl SessionToolState {
    pub fn new(
        session_id: String,
        task_notify: UnboundedSender<super::notification::TaskChange>,
        extra_read_roots: Vec<PathBuf>,
        extra_rw_roots: Vec<PathBuf>,
    ) -> Self {
        let canon = |roots: Vec<PathBuf>| {
            // Canonicalize when possible (fall back to the original path if the directory does not exist): allowlist comparison is done in canonical terms
            roots
                .into_iter()
                .map(|root| root.canonicalize().unwrap_or(root))
                .collect()
        };
        Self {
            todos: TodoHandle::default(),
            tasks: TaskRegistry::default(),
            task_notify,
            session_id,
            task_store: None,
            delivery_receipts: Arc::new(Mutex::new(HashMap::new())),
            data_dir: None,
            task_seq: Arc::new(AtomicUsize::new(0)),
            agent_steers: Arc::new(Mutex::new(HashMap::new())),
            agent_writers: Arc::new(Mutex::new(HashSet::new())),
            read_states: Arc::new(Mutex::new(HashMap::new())),
            extra_read_roots: canon(extra_read_roots),
            extra_rw_roots: canon(extra_rw_roots),
            sandbox_project: Arc::new(std::sync::RwLock::new(Default::default())),
        }
    }

    pub fn with_notification_store(
        mut self,
        store: Arc<Mutex<crate::store::Store>>,
        receipts: HashMap<String, pig_protocol::DeliveryChannel>,
    ) -> Self {
        self.delivery_receipts = Arc::new(Mutex::new(
            receipts
                .into_iter()
                .map(|(id, channel)| {
                    (
                        id,
                        super::notification::DeliveryReceipt {
                            channel,
                            confirmed: true,
                        },
                    )
                })
                .collect(),
        ));
        self.task_store = Some(store);
        self
    }

    pub fn restore_tasks(&self, tasks: Vec<BackgroundTask>) {
        for task in &tasks {
            if let Some(sequence) = task
                .summary
                .id
                .strip_prefix('b')
                .and_then(|suffix| suffix.parse::<usize>().ok())
            {
                self.task_seq.fetch_max(sequence, Ordering::Relaxed);
            }
        }
        self.tasks.restore(tasks);
    }

    pub fn notify_task(&self, task_id: &str) {
        let tasks = self.tasks.lock().expect("task registry lock");
        if let Some(change) = tasks
            .iter()
            .find(|entry| entry.id == task_id)
            .map(|entry| super::notification::snapshot_change(&self.session_id, entry))
        {
            let _ = self.task_notify.send(change);
        }
    }

    pub fn notify_terminal(&self, task_id: &str) {
        let tasks = self.tasks.lock().expect("task registry lock");
        if let Some(change) = tasks
            .iter()
            .find(|entry| entry.id == task_id)
            .map(|entry| super::notification::terminal_change(&self.session_id, entry))
        {
            let _ = self.task_notify.send(change);
        }
    }

    /// Attach the session data directory; task spills live under
    /// `{data_dir}/sessions/{session_id}/tool-results`. Session assembly calls
    /// this once, keeping the existing constructor signature untouched.
    pub fn with_data_dir(mut self, data_dir: PathBuf) -> Self {
        self.data_dir = Some(data_dir);
        self
    }

    /// Attached session data directory (tests locate spill artifacts through it).
    pub fn data_dir(&self) -> Option<&Path> {
        self.data_dir.as_deref()
    }

    /// Full-output spill path for one task id (`None` when no data directory is
    /// attached: bare test states and subagent entries never spill).
    pub fn spill_path(&self, task_id: &str) -> Option<PathBuf> {
        self.data_dir
            .as_ref()
            .map(|dir| spill::path_for(dir, &self.session_id, task_id))
    }

    /// For tests: session_id = "test"; task-change receivers are dropped
    /// outright (send failures ignored). Each state gets its own temporary data
    /// directory (so parallel tests cannot share spill files) and read roots
    /// include it alongside tmp (tmp is already exempt, so existing test
    /// behavior is unchanged).
    pub fn for_test() -> Self {
        static TEST_SEQ: AtomicUsize = AtomicUsize::new(0);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let data_dir = std::env::temp_dir().join(format!(
            "pig-task-state-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::create_dir_all(&data_dir);
        Self::new(
            "test".to_string(),
            tx,
            vec![std::env::temp_dir(), data_dir.clone()],
            vec![],
        )
        .with_data_dir(data_dir)
    }

    /// Claim exclusive write access to one subagent context. `None` means
    /// another driver still owns it (running or shutting down).
    pub fn try_claim_agent_writer(&self, agent_id: &str) -> Option<AgentWriterGuard> {
        let mut active = self
            .agent_writers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !active.insert(agent_id.to_string()) {
            return None;
        }
        Some(AgentWriterGuard {
            registry: self.agent_writers.clone(),
            agent_id: agent_id.to_string(),
            released: false,
        })
    }

    pub fn agent_writer_active(&self, agent_id: &str) -> bool {
        self.agent_writers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(agent_id)
    }

    /// task_id of a still-running registry entry for this subagent, if any.
    pub fn running_agent_task(&self, agent_id: &str) -> Option<String> {
        self.tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .find(|task| {
                task.agent_id.as_deref() == Some(agent_id)
                    && matches!(task.status, TaskStatus::Running | TaskStatus::Queued)
            })
            .map(|task| task.id.clone())
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
    registry
        .lock()
        .expect("task registry lock")
        .iter()
        .filter(|entry| !entry.foreground)
        .map(TaskEntry::summary)
        .collect()
}

impl TaskRegistry {
    pub fn restore(&self, tasks: Vec<BackgroundTask>) {
        let mut entries = self.lock().expect("task registry lock");
        for task in tasks {
            let summary = task.summary;
            entries.push(TaskEntry {
                id: summary.id,
                kind: summary.kind,
                command: summary.command,
                status: summary.status,
                exit_code: summary.exit_code,
                reason: summary.reason,
                started_at: summary.started_at_ms / 1_000,
                ended_at: summary.ended_at_ms.map(|value| value / 1_000),
                pid: None,
                output: summary.output_tail,
                spill: summary.spill_path.map(spill::Spill::restored),
                cancel: None,
                agent_id: summary.agent_id,
                record_path: task.record_path,
                result_path: task.result_path,
                turns: task.turns,
                model: task.model,
                terminal_notification_suppressed: task.terminal_notification_suppressed,
                foreground: !task.detached,
            });
        }
    }
}

/// Task sequence: b1, b2... monotonically increasing (foreground tasks are removed from the registry on completion, so len+1 risks reuse)
pub(super) fn next_task_id(seq: &AtomicUsize) -> String {
    format!("b{}", seq.fetch_add(1, Ordering::Relaxed) + 1)
}

/// Spill sink for a newly registered task: the path comes from the session's
/// data directory and task id; the file itself appears only once the output
/// crosses the spill threshold.
pub(super) fn spill_for(state: &SessionToolState, task_id: &str) -> Option<spill::Spill> {
    state.spill_path(task_id).map(spill::Spill::new)
}

/// Roll the registry output (unchanged 64KB head truncation); also feed the
/// task's spill. raw goes to the spill (byte-faithful, including incomplete
/// suspended sequences); text is the decoded view (produced by StreamDecoder,
/// falling back to GBK for non-UTF-8 output on Windows).
pub(super) fn append_output(registry: &TaskRegistry, task_id: &str, raw: &[u8], text: &str) {
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
    if let Some(spill) = &mut entry.spill {
        spill.append(raw);
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

/// Register a subagent task entry (Running, no pid/spill; cancel = the driver
/// cancellation token, agent_id for resume-while-running conflict detection
/// and panel labeling), notify immediately so the panel shows the entry, and
/// return the task_id.
pub fn register_agent_task(
    state: &SessionToolState,
    command: String,
    cancel: tokio_util::sync::CancellationToken,
    agent_id: String,
) -> String {
    register_agent_task_with_status(state, command, cancel, agent_id, TaskStatus::Running)
}

fn register_agent_task_with_status(
    state: &SessionToolState,
    command: String,
    cancel: tokio_util::sync::CancellationToken,
    agent_id: String,
    status: TaskStatus,
) -> String {
    let id = {
        let mut tasks = state.tasks.lock().expect("task registry lock");
        let id = next_task_id(&state.task_seq);
        tasks.push(TaskEntry {
            id: id.clone(),
            kind: TaskKind::Subagent,
            command,
            status,
            exit_code: None,
            reason: None,
            started_at: now_secs(),
            ended_at: None,
            pid: None,
            output: String::new(),
            spill: None,
            cancel: Some(cancel),
            agent_id: Some(agent_id),
            record_path: None,
            result_path: None,
            turns: None,
            model: None,
            terminal_notification_suppressed: false,
            foreground: false,
        });
        id
    };
    state.notify_task(&id);
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

/// Serializes tests that observe the process-wide slot count, so one test's
/// held permits cannot make another's exact-count assertion fail.
#[cfg(test)]
pub(crate) static SUBAGENT_SLOT_TEST_LOCK: tokio::sync::Mutex<()> =
    tokio::sync::Mutex::const_new(());

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
        // Cancellation wins over a freshly freed slot: a stopped queued child
        // must not briefly occupy the global permit on its way out.
        biased;
        _ = cancel.cancelled() => None,
        permit = SUBAGENT_SLOTS.acquire() => permit.ok(),
    }
}

/// Register a subagent task in the queued state and move it to Running once a
/// global concurrency slot has been acquired.
pub fn register_agent_task_queued(
    state: &SessionToolState,
    command: String,
    cancel: tokio_util::sync::CancellationToken,
    agent_id: String,
) -> String {
    register_agent_task_with_status(state, command, cancel, agent_id, TaskStatus::Queued)
}

/// Concurrency slot acquired: move the task to Running and refresh the panel.
pub fn mark_agent_task_started(state: &SessionToolState, task_id: &str) {
    {
        let mut tasks = state.tasks.lock().expect("task registry lock");
        if let Some(entry) = tasks.iter_mut().find(|entry| entry.id == task_id)
            && entry.status == TaskStatus::Queued
        {
            entry.status = TaskStatus::Running;
        }
    }
    state.notify_task(task_id);
}
