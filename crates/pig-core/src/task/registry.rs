use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pig_protocol::{TaskKind, TaskStatus, TaskSummary};

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentRunMetadata {
    pub agent_id: String,
    pub run_id: String,
    pub parent_agent_id: Option<String>,
    pub delegation_depth: u32,
    pub profile: String,
    pub usage: (u64, u64, u64),
}

pub struct TaskEntry {
    // Agent-run fields are live/durable projections of AgentRuntime. This entry
    // owns task output, cancellation and artifact paths, not Agent run authority.
    pub id: String,
    pub kind: TaskKind,
    pub command: String,
    pub status: TaskStatus,
    pub exit_code: Option<i32>,
    pub reason: Option<String>,
    pub started_at: u64,
    pub ended_at: Option<u64>,
    pub pid: Option<u32>,
    pub(super) process: Option<super::process::ProcessTask>,
    pub output: String,
    /// Full-output spill (lazily created under the session's data-dir tool-results dir)
    pub(super) spill: Option<spill::Spill>,
    /// Driver cancellation token for subagent tasks (always None for shell commands); stop_task prefers it over killing the process tree
    pub cancel: Option<tokio_util::sync::CancellationToken>,
    /// agent_id of subagent tasks (always None for shell commands); used for resume-while-running conflict detection
    pub agent_id: Option<String>,
    pub run: Option<AgentRunMetadata>,
    /// The direct agent that spawned this task; None for the root session.
    /// A running owner receives a steer; an inactive owner falls back to the
    /// root session's durable notification queue.
    pub owner_agent_id: Option<String>,
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
    pub(crate) fn needs_stop(&self) -> bool {
        matches!(self.status, TaskStatus::Running | TaskStatus::Queued)
            || self
                .process
                .as_ref()
                .is_some_and(|process| !process.is_quiescent())
    }

    pub fn recovered_agent(
        id: String,
        command: String,
        run: AgentRunMetadata,
        record_path: PathBuf,
        result_path: Option<PathBuf>,
        model: String,
        created_at: u64,
    ) -> Self {
        Self {
            id,
            kind: TaskKind::Subagent,
            command,
            status: TaskStatus::Lost,
            exit_code: None,
            reason: Some("The previous process ended before this child was registered".to_string()),
            started_at: created_at,
            ended_at: Some(now_secs()),
            pid: None,
            process: None,
            output: String::new(),
            spill: None,
            cancel: None,
            agent_id: Some(run.agent_id.clone()),
            run: Some(run),
            owner_agent_id: None,
            record_path: Some(record_path),
            result_path,
            turns: None,
            model: Some(model),
            terminal_notification_suppressed: false,
            foreground: false,
        }
    }

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
            run: self.run.clone(),
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

    #[cfg(test)]
    pub(crate) fn try_lock(&self) -> Option<RegistryGuard<'_>> {
        let guard = self.entries.try_lock().ok()?;
        Some(RegistryGuard {
            guard,
            revision: &self.revision,
            changed: false,
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

/// Registration of a root or delegated agent's explicit TaskWait interest.
/// Successfully delivered ids stay registered to prevent duplicate delivery;
/// timeout, cancellation and errors release the pending registration.
pub(crate) struct TaskWaitInterest {
    waits: Arc<Mutex<HashMap<String, HashSet<String>>>>,
    state: SessionToolState,
    owner: String,
    task_ids: Vec<String>,
    delivered: bool,
}

impl TaskWaitInterest {
    pub(crate) fn register(
        state: &SessionToolState,
        owner: &str,
        task_ids: &HashSet<String>,
    ) -> Self {
        let mut task_ids: Vec<String> = task_ids.iter().cloned().collect();
        if task_ids.is_empty() {
            task_ids.push("*".to_string());
        }
        state
            .agent_task_waits
            .lock()
            .expect("agent task waits lock")
            .entry(owner.to_string())
            .or_default()
            .extend(task_ids.iter().cloned());
        Self {
            waits: state.agent_task_waits.clone(),
            state: state.clone(),
            owner: owner.to_string(),
            task_ids,
            delivered: false,
        }
    }

    /// The wait returned these terminal notifications: keep suppression for
    /// exactly those tasks. Tasks still running lose their registration so a
    /// later terminal notification can still reach the child as a steer.
    pub(crate) fn delivered(&mut self, notifications: &[pig_protocol::TaskNotification]) {
        let delivered: HashSet<String> = notifications
            .iter()
            .map(|notification| notification.task_id.clone())
            .collect();
        let mut waits = self.waits.lock().expect("agent task waits lock");
        if let Some(ids) = waits.get_mut(&self.owner) {
            ids.retain(|id| delivered.contains(id));
            if ids.is_empty() {
                waits.remove(&self.owner);
            }
        }
        drop(waits);
        // A terminal notification may already have been queued before this
        // wait registered. It is the same fact the TaskWait result now
        // carries, so remove that queued steer instead of delivering twice.
        if !notifications.is_empty() {
            self.state.agents.retain_mailbox(&self.owner, |steer| {
                !notifications
                    .iter()
                    .any(|notification| steer.text.contains(&notification.id))
            });
        }
        for notification in notifications {
            mark_agent_notification_delivered(
                &self.state,
                &notification.task_id,
                Some(&notification.id),
            );
        }
        self.delivered = true;
    }
}

impl Drop for TaskWaitInterest {
    fn drop(&mut self) {
        if self.delivered {
            return;
        }
        let mut waits = self.waits.lock().expect("agent task waits lock");
        if let Some(ids) = waits.get_mut(&self.owner) {
            for id in &self.task_ids {
                ids.remove(id);
            }
            if ids.is_empty() {
                waits.remove(&self.owner);
            }
        }
        drop(waits);
        let changes: Vec<super::notification::TaskChange> = {
            let tasks = self.state.tasks.lock().expect("task registry lock");
            tasks
                .iter()
                .filter(|entry| {
                    entry.status.is_terminal()
                        && !entry.terminal_notification_suppressed
                        && (self.task_ids.contains(&entry.id)
                            || self.task_ids.iter().any(|id| id == "*"))
                })
                .map(|entry| super::notification::terminal_change(&self.state.session_id, entry))
                .collect()
        };
        for change in changes {
            super::notification::publish_change(&self.state.task_notify, change);
        }
    }
}

/// Whether `owner` has an active or delivered TaskWait claim for `task_id`.
pub(crate) fn task_wait_pending(state: &SessionToolState, owner: &str, task_id: &str) -> bool {
    state
        .agent_task_waits
        .lock()
        .expect("agent task waits lock")
        .get(owner)
        .is_some_and(|ids| ids.contains(task_id) || ids.contains("*"))
}

pub(crate) fn task_wait_is_pending(state: &SessionToolState, task_id: &str) -> bool {
    state
        .agent_task_waits
        .lock()
        .expect("agent task waits lock")
        .values()
        .any(|ids| ids.contains(task_id) || ids.contains("*"))
}

pub(crate) fn mark_agent_notification_delivered(
    state: &SessionToolState,
    task_id: &str,
    notification_id: Option<&str>,
) {
    let changed = {
        let mut tasks = state.tasks.lock().expect("task registry lock");
        if let Some(entry) = tasks.iter_mut().find(|entry| entry.id == task_id) {
            let changed = !entry.terminal_notification_suppressed;
            entry.terminal_notification_suppressed = true;
            changed
        } else {
            false
        }
    };
    if changed {
        state.notify_task(task_id);
    }
    if state.owner_agent_id.is_some()
        && let Some(notification_id) = notification_id
        && let Err(error) = state.agents.consume_notification(notification_id)
    {
        tracing::warn!(%task_id, %notification_id, %error, "notification mailbox consumption failed");
    }
}

/// Drain a settled agent's mailbox. Notifications that never reached its
/// durable context are re-emitted after the lifecycle transition, so the
/// manager routes them to the root fallback instead of dropping them.
pub(crate) fn release_agent_mailbox(state: &SessionToolState, owner: &str) {
    let pending = state.agents.drain_notifications(owner);
    let task_ids: HashSet<String> = pending
        .into_iter()
        .filter_map(|steer| steer.notification.map(|notification| notification.task_id))
        .collect();
    if task_ids.is_empty() {
        return;
    }
    let changes: Vec<super::notification::TaskChange> = {
        let tasks = state.tasks.lock().expect("task registry lock");
        tasks
            .iter()
            .filter(|entry| task_ids.contains(&entry.id) && !entry.terminal_notification_suppressed)
            .map(|entry| super::notification::terminal_change(&state.session_id, entry))
            .collect()
    };
    for change in changes {
        super::notification::publish_change(&state.task_notify, change);
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
    pub task_notify: async_channel::Sender<super::notification::TaskChange>,
    pub session_id: String,
    /// The subagent that owns tasks spawned through this state; None for the
    /// root session (its background tasks notify it directly).
    pub owner_agent_id: Option<String>,
    /// Live and recovered agents in this root conversation.
    pub(crate) agents: crate::agent::AgentExecutionRegistry,
    pub(crate) scheduler: Arc<crate::agent::AgentScheduler>,
    pub(crate) backends: Arc<crate::agent::AgentBackendRegistry>,
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
    /// Serializes admission with ID/writer reservation, including swarm prep.
    /// Exclusive writer guards and budget reservations live in AgentExecutionRegistry.
    pub agent_spawn_lock: Arc<Mutex<()>>,
    /// Tasks a subagent is waiting for, keyed by owner agent id. A successful
    /// TaskWait keeps the id so the terminal notification is not also queued
    /// as a steer; a timeout or error removes it.
    pub agent_task_waits: Arc<Mutex<HashMap<String, HashSet<String>>>>,
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
            owner_agent_id: self.owner_agent_id.clone(),
            agents: self.agents.clone(),
            scheduler: self.scheduler.clone(),
            backends: self.backends.clone(),
            task_store: self.task_store.clone(),
            delivery_receipts: self.delivery_receipts.clone(),
            data_dir: self.data_dir.clone(),
            task_seq: self.task_seq.clone(),
            read_states: self.read_states.clone(),
            agent_spawn_lock: self.agent_spawn_lock.clone(),
            agent_task_waits: self.agent_task_waits.clone(),
            extra_read_roots: self.extra_read_roots.clone(),
            extra_rw_roots: self.extra_rw_roots.clone(),
            sandbox_project: self.sandbox_project.clone(),
        }
    }
}

impl SessionToolState {
    pub fn new(
        session_id: String,
        task_notify: async_channel::Sender<super::notification::TaskChange>,
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
        let scheduler = Arc::new(crate::agent::AgentScheduler::new(
            crate::agent::MAX_CONCURRENT_AGENT_RUNS,
        ));
        let backends = Arc::new(crate::agent::AgentBackendRegistry::with_local());
        backends.attach_scheduler(scheduler.clone());
        Self {
            todos: TodoHandle::default(),
            tasks: TaskRegistry::default(),
            task_notify,
            session_id: session_id.clone(),
            owner_agent_id: None,
            agents: crate::agent::AgentExecutionRegistry::with_backends(
                session_id.clone(),
                backends.clone(),
            ),
            scheduler,
            backends,
            task_store: None,
            delivery_receipts: Arc::new(Mutex::new(HashMap::new())),
            data_dir: None,
            task_seq: Arc::new(AtomicUsize::new(0)),
            agent_spawn_lock: Arc::new(Mutex::new(())),
            agent_task_waits: Arc::new(Mutex::new(HashMap::new())),
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
        self.agents.attach_store(store.clone());
        self.task_store = Some(store);
        self
    }

    pub fn restore_tasks(&self, tasks: Vec<BackgroundTask>) {
        for task in &tasks {
            if let Some(run) = &task.run {
                let status = match task.summary.status {
                    TaskStatus::Queued | TaskStatus::Running => pig_protocol::AgentRunStatus::Lost,
                    TaskStatus::Completed => pig_protocol::AgentRunStatus::Completed,
                    TaskStatus::Killed => pig_protocol::AgentRunStatus::Cancelled,
                    TaskStatus::Failed | TaskStatus::TimedOut => {
                        pig_protocol::AgentRunStatus::Failed
                    }
                    TaskStatus::Lost => pig_protocol::AgentRunStatus::Lost,
                };
                let lifecycle = match task.summary.status {
                    TaskStatus::Lost | TaskStatus::Queued | TaskStatus::Running => {
                        crate::agent::AgentLifecycle::Lost
                    }
                    _ => crate::agent::AgentLifecycle::Idle,
                };
                self.agents.restore(crate::agent::AgentNode::restored(
                    run.agent_id.clone(),
                    run.parent_agent_id.clone(),
                    run.delegation_depth,
                    lifecycle,
                    Some(crate::agent::AgentRunState::new(
                        run.run_id.clone(),
                        status,
                        run.parent_agent_id.clone(),
                        run.delegation_depth,
                    )),
                ));
            }
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

    pub fn task_snapshot(&self) -> Vec<pig_protocol::TaskSummary> {
        let mut summaries = crate::task::snapshot(&self.tasks);
        let Some(runtime) = self.agents.runtime_snapshot() else {
            return summaries;
        };
        let runs: std::collections::HashMap<String, crate::agent::runtime::RuntimeRun> = runtime
            .runs
            .into_iter()
            .map(|run| (run.run_id.clone(), run))
            .collect();
        let task_runs: std::collections::HashMap<String, String> = self
            .tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .filter_map(|task| {
                task.run
                    .as_ref()
                    .map(|run| (task.id.clone(), run.run_id.clone()))
            })
            .collect();
        for summary in &mut summaries {
            let Some(run) = task_runs
                .get(&summary.id)
                .and_then(|run_id| runs.get(run_id))
            else {
                continue;
            };
            match run.status {
                crate::agent::runtime::RuntimeRunStatus::Queued => {
                    summary.status = pig_protocol::TaskStatus::Queued
                }
                crate::agent::runtime::RuntimeRunStatus::Running => {
                    summary.status = pig_protocol::TaskStatus::Running
                }
                crate::agent::runtime::RuntimeRunStatus::Stopping => {}
                crate::agent::runtime::RuntimeRunStatus::Completed => {
                    summary.status = pig_protocol::TaskStatus::Completed
                }
                crate::agent::runtime::RuntimeRunStatus::Cancelled => {
                    summary.status = pig_protocol::TaskStatus::Killed
                }
                crate::agent::runtime::RuntimeRunStatus::Failed => {
                    summary.status = pig_protocol::TaskStatus::Failed
                }
                crate::agent::runtime::RuntimeRunStatus::Lost => {
                    summary.status = pig_protocol::TaskStatus::Lost
                }
            }
            if let Some(reason) = &run.stop_reason {
                summary.reason = Some(reason.clone());
            }
            if let Some(ended_at_ms) = run.ended_at_ms {
                summary.ended_at_ms = Some(ended_at_ms);
            }
        }
        summaries
    }

    pub fn notify_task(&self, task_id: &str) {
        let change = {
            let tasks = self.tasks.lock().expect("task registry lock");
            tasks
                .iter()
                .find(|entry| entry.id == task_id)
                .map(|entry| super::notification::snapshot_change(&self.session_id, entry))
        };
        if let Some(change) = change {
            super::notification::publish_change(&self.task_notify, change);
        }
    }

    pub fn notify_terminal(&self, task_id: &str) {
        let change = {
            let tasks = self.tasks.lock().expect("task registry lock");
            tasks
                .iter()
                .find(|entry| entry.id == task_id)
                .map(|entry| super::notification::terminal_change(&self.session_id, entry))
        };
        if let Some(change) = change {
            super::notification::publish_change(&self.task_notify, change);
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
        let (tx, _rx) = async_channel::bounded(1024);
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
    pub(crate) fn try_claim_agent_writer(
        &self,
        agent_id: &str,
    ) -> Option<crate::agent::AgentWriterGuard> {
        self.agents.claim_writer(agent_id)
    }

    pub fn agent_writer_active(&self, agent_id: &str) -> bool {
        self.agents.writer_active(agent_id)
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
                process: None,
                output: summary.output_tail,
                spill: summary.spill_path.map(spill::Spill::restored),
                cancel: None,
                agent_id: summary.agent_id,
                run: task.run,
                owner_agent_id: None,
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
    if let Some(spill) = &mut entry.spill
        && let Err(error) = spill.append(raw)
    {
        tracing::warn!(task_id = %entry.id, %error, "task spill persistence failed");
        entry.reason = Some(error);
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
    register_agent_task_with_status(
        state,
        command,
        cancel,
        agent_id,
        TaskStatus::Running,
        None,
        false,
    )
}

fn register_agent_task_with_status(
    state: &SessionToolState,
    command: String,
    cancel: tokio_util::sync::CancellationToken,
    agent_id: String,
    status: TaskStatus,
    run: Option<AgentRunMetadata>,
    terminal_notification_suppressed: bool,
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
            process: None,
            output: String::new(),
            spill: None,
            cancel: Some(cancel),
            agent_id: Some(agent_id),
            run,
            owner_agent_id: state.owner_agent_id.clone(),
            record_path: None,
            result_path: None,
            turns: None,
            model: None,
            terminal_notification_suppressed,
            foreground: false,
        });
        id
    };
    state.notify_task(&id);
    id
}

/// Register a subagent task in the queued state and move it to Running once a
/// session-tree concurrency slot has been acquired.
pub fn register_agent_task_queued(
    state: &SessionToolState,
    command: String,
    cancel: tokio_util::sync::CancellationToken,
    agent_id: String,
) -> String {
    register_agent_task_with_status(
        state,
        command,
        cancel,
        agent_id,
        TaskStatus::Queued,
        None,
        false,
    )
}

pub fn register_agent_run_queued(
    state: &SessionToolState,
    command: String,
    cancel: tokio_util::sync::CancellationToken,
    run: AgentRunMetadata,
    background: bool,
) -> String {
    register_agent_task_with_status(
        state,
        command,
        cancel,
        run.agent_id.clone(),
        TaskStatus::Queued,
        Some(run),
        !background,
    )
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
