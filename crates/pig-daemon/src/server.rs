use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use fs2::FileExt as _;
use futures_util::{SinkExt as _, StreamExt as _};
use pig_core::{AgentHandle, Projected, ProjectionRegistry, ProjectionResync};
use pig_protocol::{
    ContextBreakdown, ContextUsageSnapshot, Event, EventCursor, EventEnvelope, Op,
    PendingApprovalRequest, PendingQuestionRequest, SessionStateSnapshot, TaskNotification,
    TaskSummary, TodoItem,
};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;

use crate::{
    DAEMON_HISTORY, DAEMON_PROTOCOL_VERSION, DaemonEndpoint, DaemonMessage, DaemonRequest,
    ProjectedEventBatch,
};

const SUBSCRIBER_CHANNEL_CAPACITY: usize = DAEMON_HISTORY + 64;
const PONG_CHANNEL_CAPACITY: usize = 16;

#[derive(Clone, Debug)]
pub struct DaemonConfig {
    pub data_dir: PathBuf,
    pub cwd: PathBuf,
    pub config_path: Option<PathBuf>,
    pub selftest: bool,
}

#[derive(Default)]
struct SessionOverlay {
    ready: bool,
    configuration: Option<pig_protocol::SessionConfigurationSnapshot>,
    pending_queue: Vec<pig_protocol::QueueItem>,
    steer_queue: Vec<pig_protocol::QueueItem>,
    approvals: HashMap<String, PendingApprovalRequest>,
    questions: HashMap<String, PendingQuestionRequest>,
    todos: Vec<TodoItem>,
    tasks: Vec<TaskSummary>,
    task_notifications: Vec<TaskNotification>,
    context_usage: Option<ContextUsageSnapshot>,
    context_breakdown: Option<ContextBreakdown>,
}

impl SessionOverlay {
    fn apply(&mut self, event: &Event) {
        match event {
            Event::QueueChanged { items, .. } => self.pending_queue = items.clone(),
            Event::SteerChanged { items, .. } => self.steer_queue = items.clone(),
            Event::PermissionsChanged { permissions, .. } => {
                if let Some(configuration) = self.configuration.as_mut() {
                    configuration.permissions = *permissions;
                }
            }
            Event::PlanModeChanged { enabled, .. } => {
                if let Some(configuration) = self.configuration.as_mut() {
                    configuration.plan_enabled = *enabled;
                }
            }
            Event::TodoListChanged { items, .. } => self.todos = items.clone(),
            Event::TaskListChanged { tasks, .. } => self.tasks = tasks.clone(),
            Event::TaskNotificationQueueChanged { notifications, .. } => {
                self.task_notifications = notifications.clone();
            }
            Event::ContextUsage {
                used,
                total,
                cache_read_total,
                input_total,
                ..
            } => {
                self.context_usage = Some(ContextUsageSnapshot {
                    used: *used,
                    total: *total,
                    cache_read_total: *cache_read_total,
                    input_total: *input_total,
                });
            }
            Event::ContextBreakdownChanged { breakdown, .. } => {
                self.context_breakdown = Some(breakdown.clone());
            }
            Event::ApprovalRequested {
                request_id,
                tool,
                detail,
                danger_key,
                requested_write_roots,
                available_decisions,
                ..
            } => {
                self.approvals.insert(
                    request_id.clone(),
                    PendingApprovalRequest {
                        request_id: request_id.clone(),
                        tool: tool.clone(),
                        detail: detail.clone(),
                        danger_key: danger_key.clone(),
                        requested_write_roots: requested_write_roots.clone(),
                        available_decisions: available_decisions.clone(),
                    },
                );
            }
            Event::ApprovalResolved { request_id, .. } => {
                self.approvals.remove(request_id);
            }
            Event::QuestionRequested {
                request_id,
                questions,
                ..
            } => {
                self.questions.insert(
                    request_id.clone(),
                    PendingQuestionRequest {
                        request_id: request_id.clone(),
                        questions: questions.clone(),
                    },
                );
            }
            Event::QuestionResolved { request_id, .. } => {
                self.questions.remove(request_id);
            }
            Event::TurnAborted { .. } => {
                self.approvals.clear();
                self.questions.clear();
            }
            _ => {}
        }
    }
}

struct Subscriber {
    sender: async_channel::Sender<DaemonMessage>,
    pending_attach: Option<String>,
    session_filter: Option<HashSet<String>>,
}

impl Subscriber {
    fn accepts_session(&self, session_id: Option<&str>) -> bool {
        let Some(allowed) = &self.session_filter else {
            return true;
        };
        session_id.is_none_or(|id| allowed.contains(id))
    }
}

struct SharedState {
    epoch: String,
    latest: u64,
    history: VecDeque<ProjectedEventBatch>,
    projection: ProjectionRegistry,
    sessions: HashMap<String, SessionOverlay>,
    loading_sessions: HashSet<String>,
    subscribers: HashMap<u64, Subscriber>,
    next_subscriber: u64,
}

impl SharedState {
    fn new(epoch: String) -> Self {
        Self {
            epoch,
            latest: 0,
            history: VecDeque::with_capacity(DAEMON_HISTORY),
            projection: ProjectionRegistry::new(),
            sessions: HashMap::new(),
            loading_sessions: HashSet::new(),
            subscribers: HashMap::new(),
            next_subscriber: 0,
        }
    }

    fn cursor(&self) -> EventCursor {
        EventCursor {
            epoch: self.epoch.clone(),
            sequence: self.latest,
        }
    }

    fn subscribe(
        &mut self,
        after: Option<EventCursor>,
        session_ids: Option<Vec<String>>,
        sender: async_channel::Sender<DaemonMessage>,
    ) -> u64 {
        self.next_subscriber += 1;
        let id = self.next_subscriber;
        let session_filter = session_ids.map(|ids| ids.into_iter().collect::<HashSet<_>>());
        let oldest = self
            .history
            .front()
            .map_or(self.latest.saturating_add(1), |batch| batch.sequence);
        let can_replay = after.as_ref().is_some_and(|cursor| {
            cursor.epoch == self.epoch
                && cursor.sequence <= self.latest
                && cursor.sequence >= oldest.saturating_sub(1)
        });
        let resync_required = after.is_some() && !can_replay;
        let start = if can_replay {
            after.clone().expect("checked replay cursor")
        } else {
            self.cursor()
        };
        let replay = if can_replay {
            let sequence = start.sequence;
            self.history
                .iter()
                .filter(|batch| {
                    batch.sequence > sequence
                        && session_filter.as_ref().is_none_or(|allowed| {
                            batch
                                .session_id
                                .as_deref()
                                .is_none_or(|sid| allowed.contains(sid))
                        })
                })
                .cloned()
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let mut delivered = sender
            .try_send(DaemonMessage::Subscription {
                cursor: start,
                resync_required,
            })
            .is_ok();
        if delivered {
            for batch in replay {
                if sender.try_send(DaemonMessage::Batch(batch)).is_err() {
                    delivered = false;
                    break;
                }
            }
        }
        if delivered {
            self.subscribers.insert(
                id,
                Subscriber {
                    sender,
                    pending_attach: None,
                    session_filter,
                },
            );
        }
        id
    }

    fn unsubscribe(&mut self, id: u64) {
        self.subscribers.remove(&id);
    }

    fn note_op(&mut self, op: &Op) {
        match op {
            Op::SetPermissions {
                session_id,
                permissions,
            } => {
                if let Some(configuration) = self
                    .sessions
                    .get_mut(session_id)
                    .and_then(|overlay| overlay.configuration.as_mut())
                {
                    configuration.permissions = *permissions;
                }
            }
            Op::SetPlanMode {
                session_id,
                enabled,
            } => {
                if let Some(configuration) = self
                    .sessions
                    .get_mut(session_id)
                    .and_then(|overlay| overlay.configuration.as_mut())
                {
                    configuration.plan_enabled = *enabled;
                }
            }
            Op::SetModel {
                session_id,
                provider_id,
                model_id,
                reasoning_level,
            } => {
                if let Some(configuration) = self
                    .sessions
                    .get_mut(session_id)
                    .and_then(|overlay| overlay.configuration.as_mut())
                {
                    configuration.provider_id = Some(provider_id.clone());
                    configuration.model_id = Some(model_id.clone());
                    configuration.model = model_id.clone();
                    configuration.reasoning_level = reasoning_level.clone();
                }
            }
            Op::SetReasoning {
                session_id,
                reasoning_level,
            } => {
                if let Some(configuration) = self
                    .sessions
                    .get_mut(session_id)
                    .and_then(|overlay| overlay.configuration.as_mut())
                {
                    configuration.reasoning_level = reasoning_level.clone();
                }
            }
            Op::DeleteSession { session_id } => {
                self.sessions.remove(session_id);
                self.loading_sessions.remove(session_id);
                for subscriber in self.subscribers.values_mut() {
                    if subscriber.pending_attach.as_deref() == Some(session_id.as_str()) {
                        subscriber.pending_attach = None;
                    }
                }
            }
            _ => {}
        }
    }

    fn snapshot(&self, session_id: &str) -> SessionStateSnapshot {
        let overlay = self.sessions.get(session_id);
        SessionStateSnapshot {
            session_id: session_id.to_string(),
            configuration: overlay.and_then(|state| state.configuration.clone()),
            items: self.projection.session_items(session_id),
            pending_queue: overlay.map_or_else(Vec::new, |state| state.pending_queue.clone()),
            steer_queue: overlay.map_or_else(Vec::new, |state| state.steer_queue.clone()),
            pending_approvals: overlay.map_or_else(Vec::new, |state| {
                let mut approvals = state.approvals.values().cloned().collect::<Vec<_>>();
                approvals.sort_by(|left, right| left.request_id.cmp(&right.request_id));
                approvals
            }),
            pending_questions: overlay.map_or_else(Vec::new, |state| {
                let mut questions = state.questions.values().cloned().collect::<Vec<_>>();
                questions.sort_by(|left, right| left.request_id.cmp(&right.request_id));
                questions
            }),
            todos: overlay.map_or_else(Vec::new, |state| state.todos.clone()),
            tasks: overlay.map_or_else(Vec::new, |state| state.tasks.clone()),
            task_notifications: overlay
                .map_or_else(Vec::new, |state| state.task_notifications.clone()),
            context_usage: overlay.and_then(|state| state.context_usage.clone()),
            context_breakdown: overlay.and_then(|state| state.context_breakdown.clone()),
            running: self.projection.session_running(session_id),
            cursor: self.cursor(),
        }
    }

    fn begin_attach(&mut self, subscriber_id: u64, session_id: &str) -> bool {
        if let Some(subscriber) = self.subscribers.get_mut(&subscriber_id)
            && let Some(allowed) = subscriber.session_filter.as_mut()
        {
            allowed.insert(session_id.to_string());
        }
        if self
            .sessions
            .get(session_id)
            .is_some_and(|session| session.ready)
        {
            let snapshot = self.snapshot(session_id);
            let mut keep = true;
            if let Some(subscriber) = self.subscribers.get_mut(&subscriber_id) {
                subscriber.pending_attach = None;
                keep = subscriber
                    .sender
                    .try_send(DaemonMessage::Snapshot(snapshot))
                    .is_ok();
            }
            if !keep {
                self.subscribers.remove(&subscriber_id);
            }
            return false;
        }
        if let Some(subscriber) = self.subscribers.get_mut(&subscriber_id) {
            subscriber.pending_attach = Some(session_id.to_string());
        }
        self.loading_sessions.insert(session_id.to_string())
    }

    fn apply_session_event(&mut self, event: &Event, replaying_snapshot: bool) {
        let Some(session_id) = event.session_id() else {
            return;
        };
        if let Event::SessionConfigured {
            cwd,
            model,
            provider_name,
            provider_id,
            model_id,
            reasoning_level,
            permissions,
            plan_enabled,
            ..
        } = event
        {
            let loading = !replaying_snapshot && self.loading_sessions.contains(session_id);
            let ready = !replaying_snapshot && !loading;
            let configuration = pig_protocol::SessionConfigurationSnapshot {
                cwd: cwd.clone(),
                model: model.clone(),
                provider_name: provider_name.clone(),
                provider_id: provider_id.clone(),
                model_id: model_id.clone(),
                reasoning_level: reasoning_level.clone(),
                permissions: *permissions,
                plan_enabled: *plan_enabled,
            };
            let overlay = self.sessions.entry(session_id.to_string()).or_default();
            // Core emits QueueChanged and SteerChanged right before SessionConfigured
            // when loading a persisted session from disk; preserve them while loading.
            let (pending_queue, steer_queue) = if loading {
                (
                    std::mem::take(&mut overlay.pending_queue),
                    std::mem::take(&mut overlay.steer_queue),
                )
            } else {
                (Vec::new(), Vec::new())
            };
            *overlay = SessionOverlay {
                ready,
                configuration: Some(configuration),
                pending_queue,
                steer_queue,
                ..SessionOverlay::default()
            };
            return;
        }
        let overlay = self.sessions.entry(session_id.to_string()).or_default();
        overlay.apply(event);
        if !replaying_snapshot && matches!(event, Event::SessionReplayComplete { .. }) {
            overlay.ready = true;
            self.loading_sessions.remove(session_id);
        }
    }

    fn publish_envelope(&mut self, envelope: &EventEnvelope) -> Option<ProjectionResync> {
        if envelope.epoch == self.epoch && envelope.sequence <= self.latest {
            return None;
        }
        let projected = self.projection.project(envelope);
        if let Some(resync) = projected.resync {
            return Some(resync);
        }
        self.publish_projected(envelope, projected);
        None
    }

    fn publish_projected(&mut self, envelope: &EventEnvelope, projected: Projected) {
        self.apply_session_event(&envelope.event, false);
        self.epoch = envelope.epoch.clone();
        self.latest = envelope.sequence;
        let mut events = projected.events;
        if projected.pass {
            events.push(envelope.event.clone());
        }
        let batch = ProjectedEventBatch {
            epoch: envelope.epoch.clone(),
            sequence: envelope.sequence,
            session_id: envelope.session_id.clone(),
            events,
        };
        self.history.push_back(batch.clone());
        if self.history.len() > DAEMON_HISTORY {
            self.history.pop_front();
        }
        let replay_complete = matches!(envelope.event, Event::SessionReplayComplete { .. });
        let completed_snapshot = replay_complete
            .then(|| envelope.session_id.as_ref().map(|id| self.snapshot(id)))
            .flatten();
        let is_error = matches!(envelope.event, Event::Error { .. });
        if is_error && let Some(error_session) = envelope.session_id.as_deref() {
            self.loading_sessions.remove(error_session);
        }
        self.subscribers.retain(|_, subscriber| {
            let attaching = subscriber.pending_attach.as_deref();
            let message = if attaching.is_some()
                && attaching == envelope.session_id.as_deref()
                && replay_complete
            {
                subscriber.pending_attach = None;
                completed_snapshot.clone().map(DaemonMessage::Snapshot)
            } else if attaching.is_some()
                && attaching == envelope.session_id.as_deref()
                && is_error
            {
                subscriber.pending_attach = None;
                Some(DaemonMessage::Batch(batch.clone()))
            } else if attaching.is_some() && attaching == envelope.session_id.as_deref() {
                None
            } else if !subscriber.accepts_session(envelope.session_id.as_deref()) {
                None
            } else {
                Some(DaemonMessage::Batch(batch.clone()))
            };
            if let Some(message) = message {
                subscriber.sender.try_send(message).is_ok()
            } else {
                true
            }
        });
    }

    fn reset_with_snapshot(&mut self, snapshot: pig_core::JournalSnapshot) -> Result<(), String> {
        self.projection.reset_with_snapshot(&snapshot)?;
        self.epoch = snapshot.epoch;
        self.latest = snapshot.cursor;
        self.history.clear();
        self.sessions.clear();
        self.loading_sessions.clear();
        for envelope in &snapshot.events {
            self.apply_session_event(&envelope.event, true);
        }
        let cursor = self.cursor();
        self.subscribers.retain(|_, subscriber| {
            subscriber.pending_attach = None;
            subscriber
                .sender
                .try_send(DaemonMessage::Subscription {
                    cursor: cursor.clone(),
                    resync_required: true,
                })
                .is_ok()
        });
        Ok(())
    }
}

struct EndpointGuard(PathBuf);

impl Drop for EndpointGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn daemon_directory(data_dir: &Path) -> PathBuf {
    data_dir.join("daemon")
}

fn endpoint_path(data_dir: &Path) -> PathBuf {
    daemon_directory(data_dir).join("endpoint.json")
}

#[cfg(windows)]
fn restrict_windows_permissions(path: &Path) {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt as _;

    use windows::Win32::Foundation::{BOOL, CloseHandle, HANDLE, HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SDDL_REVISION_1, SE_FILE_OBJECT, SetNamedSecurityInfoW,
    };
    use windows::Win32::Security::{
        ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, GetTokenInformation,
        PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, TOKEN_QUERY, TOKEN_USER,
        TokenUser,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    use windows::core::{PCWSTR, PWSTR};

    unsafe fn apply(path: &Path) -> Result<(), String> {
        let mut token = HANDLE::default();
        unsafe {
            OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)
                .map_err(|error| error.to_string())?;
        }
        let mut required = 0_u32;
        let _ = unsafe { GetTokenInformation(token, TokenUser, None, 0, &mut required) };
        if required == 0 {
            unsafe {
                let _ = CloseHandle(token);
            }
            return Err("GetTokenInformation returned zero length".to_string());
        }
        let mut buffer = vec![0_u8; required as usize];
        let info_result = unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                Some(buffer.as_mut_ptr().cast()),
                required,
                &mut required,
            )
            .map_err(|error| error.to_string())
        };
        unsafe {
            let _ = CloseHandle(token);
        }
        info_result?;
        let user = unsafe { &*(buffer.as_ptr().cast::<TOKEN_USER>()) };
        let mut sid_pwstr = PWSTR::null();
        unsafe {
            ConvertSidToStringSidW(user.User.Sid, &mut sid_pwstr)
                .map_err(|error| error.to_string())?;
        }
        let sid_string = unsafe { sid_pwstr.to_string() }.map_err(|error| error.to_string());
        unsafe {
            let _ = LocalFree(HLOCAL(sid_pwstr.0.cast()));
        }
        let sid_string = sid_string?;

        // Protected DACL granting full access only to the current user and LocalSystem.
        let sddl = format!("D:P(A;OICI;FA;;;{sid_string})(A;OICI;FA;;;SY)");
        let sddl_wide: Vec<u16> = OsStr::new(&sddl).encode_wide().chain(Some(0)).collect();
        let mut sd = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(sddl_wide.as_ptr()),
                SDDL_REVISION_1,
                &mut sd,
                None,
            )
            .map_err(|error| error.to_string())?;
        }

        let mut dacl_present = BOOL(0);
        let mut dacl_defaulted = BOOL(0);
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let result = unsafe {
            GetSecurityDescriptorDacl(sd, &mut dacl_present, &mut dacl, &mut dacl_defaulted)
                .map_err(|error| error.to_string())
                .and_then(|()| {
                    if !dacl_present.as_bool() || dacl.is_null() {
                        return Err("security descriptor did not contain a DACL".to_string());
                    }
                    let path_wide: Vec<u16> =
                        path.as_os_str().encode_wide().chain(Some(0)).collect();
                    let status = SetNamedSecurityInfoW(
                        PCWSTR(path_wide.as_ptr()),
                        SE_FILE_OBJECT,
                        DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                        None,
                        None,
                        Some(dacl),
                        None,
                    );
                    if status.0 != 0 {
                        return Err(format!("SetNamedSecurityInfoW failed: {}", status.0));
                    }
                    Ok(())
                })
        };
        unsafe {
            let _ = LocalFree(HLOCAL(sd.0));
        }
        result
    }

    if let Err(error) = unsafe { apply(path) } {
        tracing::warn!(%error, path = %path.display(), "failed to restrict Windows ACL for daemon path");
    }
}

fn write_endpoint(data_dir: &Path, endpoint: &DaemonEndpoint) -> Result<EndpointGuard, String> {
    let directory = daemon_directory(data_dir);
    std::fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| error.to_string())?;
    }
    #[cfg(windows)]
    restrict_windows_permissions(&directory);
    let path = endpoint_path(data_dir);
    let temporary = directory.join(format!("endpoint-{}.tmp", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .map_err(|error| error.to_string())?;
    let encoded = serde_json::to_vec(endpoint).map_err(|error| error.to_string())?;
    file.write_all(&encoded)
        .map_err(|error| error.to_string())?;
    file.sync_all().map_err(|error| error.to_string())?;
    drop(file);
    std::fs::rename(&temporary, &path).map_err(|error| error.to_string())?;
    #[cfg(windows)]
    restrict_windows_permissions(&path);
    Ok(EndpointGuard(path))
}

fn acquire_instance_lock(data_dir: &Path) -> Result<std::fs::File, String> {
    let directory = daemon_directory(data_dir);
    std::fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(directory.join("daemon.lock"))
        .map_err(|error| error.to_string())?;
    file.try_lock_exclusive()
        .map_err(|_| "another pig-daemon already owns this data directory".to_string())?;
    Ok(file)
}

fn random_token() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate = signal(SignalKind::terminate()).ok();
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = async {
                if let Some(stream) = terminate.as_mut() {
                    stream.recv().await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

pub async fn serve(config: DaemonConfig) -> Result<(), String> {
    let _instance_lock = acquire_instance_lock(&config.data_dir)?;
    let credentials = if config.selftest || std::env::var_os("PIG_SELFTEST").is_some() {
        Arc::new(pig_core::credentials::CredentialManager::mock())
    } else {
        Arc::new(pig_core::credentials::CredentialManager::system(
            &config.data_dir,
        ))
    };
    let agent = Arc::new(pig_core::spawn_agent_with_data_dir_and_credentials(
        config.config_path.clone(),
        config.cwd.clone(),
        config.data_dir.clone(),
        credentials,
    ));
    agent.events.close();
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|error| error.to_string())?;
    let endpoint = DaemonEndpoint {
        protocol: DAEMON_PROTOCOL_VERSION,
        address: listener.local_addr().map_err(|error| error.to_string())?,
        token: random_token(),
        pid: std::process::id(),
    };
    let _endpoint_guard = write_endpoint(&config.data_dir, &endpoint)?;
    let state = Arc::new(Mutex::new(SharedState::new(
        agent.journal.epoch().to_string(),
    )));
    tokio::spawn(run_core(agent.clone(), state.clone()));
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    loop {
        let accepted = tokio::select! {
            _ = &mut shutdown => break,
            accepted = listener.accept() => accepted,
        };
        let (stream, _) = accepted.map_err(|error| error.to_string())?;
        let agent = agent.clone();
        let state = state.clone();
        let token = endpoint.token.clone();
        tokio::spawn(async move {
            if let Err(error) = handle_connection(stream, agent, state, token).await {
                tracing::debug!(%error, "daemon client disconnected");
            }
        });
    }
    let _ = agent.ops.try_send(Op::Shutdown);
    Ok(())
}

async fn run_core(agent: Arc<AgentHandle>, state: Arc<Mutex<SharedState>>) {
    let journal = agent.journal.clone();
    while let Ok(envelope) = journal.recv().await {
        let resync = state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .publish_envelope(&envelope);
        let Some(resync) = resync else {
            continue;
        };
        match journal.replay_or_snapshot(&envelope.epoch, resync.cursor) {
            Ok(missed) => {
                for envelope in missed {
                    let resync = state
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .publish_envelope(&envelope);
                    if resync.is_some() {
                        tracing::warn!("journal replay did not restore daemon projection order");
                        break;
                    }
                }
            }
            Err(snapshot) => {
                let session_id = resync.session_id.clone();
                let result = {
                    let mut guard = state
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    let result = guard.reset_with_snapshot(snapshot);
                    if result.is_ok()
                        && let Some(session_id) = &session_id
                    {
                        guard.loading_sessions.insert(session_id.clone());
                    }
                    result
                };
                if let Err(error) = result {
                    tracing::warn!(%error, "failed to reset daemon projection");
                    continue;
                }
                if let Some(session_id) = session_id {
                    let _ = agent.ops.try_send(Op::OpenSession { session_id });
                }
            }
        }
    }
}

async fn handle_connection(
    stream: TcpStream,
    agent: Arc<AgentHandle>,
    state: Arc<Mutex<SharedState>>,
    token: String,
) -> Result<(), String> {
    let expected = format!("Bearer {token}");
    let mut websocket =
        tokio_tungstenite::accept_hdr_async(stream, |request: &Request, response: Response| {
            let authorized = request
                .headers()
                .get(AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                == Some(expected.as_str());
            if authorized {
                Ok(response)
            } else {
                let mut response = ErrorResponse::new(Some("unauthorized".to_string()));
                *response.status_mut() = StatusCode::UNAUTHORIZED;
                Err(response)
            }
        })
        .await
        .map_err(|error| error.to_string())?;
    let subscribe_request = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let frame = websocket
                .next()
                .await
                .ok_or("daemon connection closed before subscription")?
                .map_err(|error| error.to_string())?;
            match frame {
                Message::Ping(payload) => {
                    websocket
                        .send(Message::Pong(payload))
                        .await
                        .map_err(|error| error.to_string())?;
                }
                Message::Pong(_) => continue,
                other => return parse_request(other),
            }
        }
    })
    .await
    .map_err(|_| "timed out waiting for daemon subscription".to_string())??;
    let DaemonRequest::Subscribe { after, session_ids } = subscribe_request else {
        return Err("daemon subscription must be the first request".to_string());
    };
    let (mut writer, mut reader) = websocket.split();
    let (outgoing, receiver) = async_channel::bounded::<DaemonMessage>(SUBSCRIBER_CHANNEL_CAPACITY);
    let (pong_tx, pong_rx) = async_channel::bounded::<Vec<u8>>(PONG_CHANNEL_CAPACITY);
    let subscriber_id = state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .subscribe(after, session_ids, outgoing);
    let mut write_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                pong = pong_rx.recv() => {
                    if let Ok(payload) = pong {
                        writer
                            .send(Message::Pong(payload.into()))
                            .await
                            .map_err(|error| error.to_string())?;
                    }
                }
                message = receiver.recv() => {
                    let Ok(message) = message else {
                        break;
                    };
                    let encoded = serde_json::to_string(&message).map_err(|error| error.to_string())?;
                    writer
                        .send(Message::text(encoded))
                        .await
                        .map_err(|error| error.to_string())?;
                }
            }
        }
        Ok::<(), String>(())
    });
    let result = loop {
        tokio::select! {
            biased;
            written = &mut write_task => {
                match written {
                    Ok(Ok(())) => break Ok(()),
                    Ok(Err(error)) => break Err(error),
                    Err(error) => break Err(error.to_string()),
                }
            }
            frame = reader.next() => {
                let Some(message) = frame else {
                    break Ok(());
                };
                let message = match message {
                    Ok(message) => message,
                    Err(error) => break Err(error.to_string()),
                };
                match message {
                    Message::Ping(payload) => {
                        let _ = pong_tx.try_send(payload.to_vec());
                        continue;
                    }
                    Message::Pong(_) => continue,
                    Message::Close(_) => break Ok(()),
                    other => {
                        let request = match parse_request(other) {
                            Ok(request) => request,
                            Err(error) => break Err(error),
                        };
                        let DaemonRequest::Op { op } = request else {
                            continue;
                        };
                        if matches!(op, Op::Shutdown) {
                            let _ = state
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner())
                                .subscribers
                                .get(&subscriber_id)
                                .map(|subscriber| {
                                    subscriber.sender.try_send(DaemonMessage::Error {
                                        detail: "daemon shutdown is not tied to a client"
                                            .to_string(),
                                    })
                                });
                            continue;
                        }
                        let should_forward = if let Op::OpenSession { session_id } = &op {
                            state
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner())
                                .begin_attach(subscriber_id, session_id)
                        } else {
                            state
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner())
                                .note_op(&op);
                            true
                        };
                        if should_forward
                            && let Err(error) = agent.ops.send(op).await
                        {
                            break Err(error.to_string());
                        }
                    }
                }
            }
        }
    };
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .unsubscribe(subscriber_id);
    write_task.abort();
    result
}

fn parse_request(message: Message) -> Result<DaemonRequest, String> {
    match message {
        Message::Text(text) => serde_json::from_str(&text).map_err(|error| error.to_string()),
        Message::Binary(bytes) => serde_json::from_slice(&bytes).map_err(|error| error.to_string()),
        Message::Close(_) => Err("daemon client closed".to_string()),
        _ => Err("expected a JSON daemon request".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(sequence: u64, event: Event) -> EventEnvelope {
        EventEnvelope {
            epoch: "test-epoch".to_string(),
            sequence,
            session_id: event.session_id().map(str::to_string),
            agent_id: None,
            run_id: None,
            event,
        }
    }

    fn configured(session_id: &str) -> Event {
        Event::SessionConfigured {
            session_id: session_id.to_string(),
            cwd: PathBuf::from("/workspace"),
            model: "model".to_string(),
            provider_name: "provider".to_string(),
            provider_id: Some("provider".to_string()),
            model_id: Some("model".to_string()),
            reasoning_level: None,
            permissions: pig_protocol::PermissionSettings::default(),
            plan_enabled: false,
        }
    }

    fn user(session_id: &str, text: &str) -> Event {
        Event::UserMessage {
            session_id: session_id.to_string(),
            text: text.to_string(),
            files: Vec::new(),
            image_nums: Vec::new(),
            sent_at_ms: 1,
        }
    }

    #[test]
    fn first_attach_switches_from_replay_to_one_atomic_snapshot() {
        let mut state = SharedState::new("test-epoch".to_string());
        let (sender, receiver) = async_channel::bounded(64);
        let subscriber = state.subscribe(None, None, sender);
        assert!(matches!(
            receiver.try_recv(),
            Ok(DaemonMessage::Subscription { .. })
        ));
        assert!(state.begin_attach(subscriber, "s1"));
        for envelope in [
            envelope(1, configured("s1")),
            envelope(2, user("s1", "hello")),
            envelope(
                3,
                Event::ApprovalRequested {
                    session_id: "s1".to_string(),
                    request_id: "approval-1".to_string(),
                    tool: "ExecCommand".to_string(),
                    detail: "command".to_string(),
                    danger_key: None,
                    requested_write_roots: Vec::new(),
                    available_decisions: Vec::new(),
                },
            ),
            envelope(
                4,
                Event::TodoListChanged {
                    session_id: "s1".to_string(),
                    items: vec![TodoItem {
                        content: "ship fix".to_string(),
                        status: pig_protocol::TodoStatus::InProgress,
                    }],
                },
            ),
            envelope(
                5,
                Event::ContextUsage {
                    session_id: "s1".to_string(),
                    used: 1200,
                    total: 128000,
                    cache_read_total: 400,
                    input_total: 800,
                },
            ),
            envelope(
                6,
                Event::PlanModeChanged {
                    session_id: "s1".to_string(),
                    enabled: true,
                    deferred: false,
                },
            ),
            envelope(
                7,
                Event::SessionReplayComplete {
                    session_id: "s1".to_string(),
                },
            ),
        ] {
            assert!(state.publish_envelope(&envelope).is_none());
        }
        let Ok(DaemonMessage::Snapshot(snapshot)) = receiver.try_recv() else {
            panic!("attaching client must receive a snapshot, not replay batches");
        };
        assert_eq!(snapshot.items.len(), 1);
        assert_eq!(snapshot.pending_approvals.len(), 1);
        assert_eq!(snapshot.todos.len(), 1);
        assert_eq!(snapshot.context_usage.as_ref().map(|u| u.used), Some(1200));
        assert_eq!(
            snapshot.configuration.as_ref().map(|c| c.plan_enabled),
            Some(true)
        );
        assert_eq!(snapshot.cursor.sequence, 7);
        assert!(receiver.try_recv().is_err());

        let (second_sender, second_receiver) = async_channel::bounded(64);
        let second = state.subscribe(None, None, second_sender);
        assert!(!state.begin_attach(second, "s1"));
        assert!(matches!(
            second_receiver.try_recv(),
            Ok(DaemonMessage::Subscription { .. })
        ));
        let Ok(DaemonMessage::Snapshot(second_snapshot)) = second_receiver.try_recv() else {
            panic!("ready sessions attach directly from a snapshot");
        };
        assert_eq!(second_snapshot.cursor.sequence, 7);
        assert_eq!(second_snapshot.items.len(), 1);
        assert_eq!(second_snapshot.todos.len(), 1);
        assert_eq!(
            second_snapshot.context_usage.as_ref().map(|u| u.used),
            Some(1200)
        );

        assert!(
            state
                .publish_envelope(&envelope(
                    8,
                    Event::ApprovalResolved {
                        session_id: "s1".to_string(),
                        request_id: "approval-1".to_string(),
                    },
                ))
                .is_none()
        );
        for events in [&receiver, &second_receiver] {
            let Ok(DaemonMessage::Batch(batch)) = events.try_recv() else {
                panic!("both clients receive the resolved event");
            };
            assert_eq!(batch.sequence, 8);
            assert!(matches!(
                batch.events.as_slice(),
                [Event::ApprovalResolved { request_id, .. }] if request_id == "approval-1"
            ));
        }
        assert!(state.snapshot("s1").pending_approvals.is_empty());
    }

    #[test]
    fn concurrent_new_session_and_open_session_do_not_corrupt_ready_state() {
        let mut state = SharedState::new("test-epoch".to_string());
        let (sender, receiver) = async_channel::bounded(64);
        let subscriber = state.subscribe(None, None, sender);
        let _ = receiver.try_recv();

        // Client starts attaching to persisted session s-old while another client creates s-new.
        assert!(state.begin_attach(subscriber, "s-old"));
        state.note_op(&Op::NewSession {
            cwd: PathBuf::from("/workspace"),
            provider_id: None,
            model_id: None,
            reasoning_level: None,
            permissions: None,
            plan_enabled: None,
        });
        // Even if s-old configures before s-new, s-old stays loading until SessionReplayComplete.
        assert!(state.publish_envelope(&envelope(1, configured("s-old"))).is_none());
        assert!(!state.sessions.get("s-old").expect("s-old overlay").ready);
        // s-new was never in loading_sessions so it becomes ready immediately.
        assert!(state.publish_envelope(&envelope(2, configured("s-new"))).is_none());
        assert!(state.sessions.get("s-new").expect("s-new overlay").ready);
        // Completing s-old replay delivers the snapshot cleanly.
        assert!(
            state
                .publish_envelope(&envelope(
                    3,
                    Event::SessionReplayComplete {
                        session_id: "s-old".to_string(),
                    },
                ))
                .is_none()
        );
        assert!(state.sessions.get("s-old").expect("s-old overlay").ready);
        let mut saw_snapshot = false;
        while let Ok(message) = receiver.try_recv() {
            if let DaemonMessage::Snapshot(snapshot) = message {
                assert_eq!(snapshot.session_id, "s-old");
                saw_snapshot = true;
            }
        }
        assert!(saw_snapshot);
    }

    #[test]
    fn failed_open_session_clears_pending_attach_and_loading_state() {
        let mut state = SharedState::new("test-epoch".to_string());
        let (sender, receiver) = async_channel::bounded(64);
        let subscriber = state.subscribe(None, None, sender);
        let _ = receiver.try_recv();

        assert!(state.begin_attach(subscriber, "missing"));
        assert!(state.loading_sessions.contains("missing"));
        assert!(
            state
                .publish_envelope(&envelope(
                    1,
                    Event::Error {
                        session_id: Some("missing".to_string()),
                        error: pig_protocol::CoreError::SessionNotFound,
                        fatal: false,
                    },
                ))
                .is_none()
        );
        assert!(!state.loading_sessions.contains("missing"));
        assert_eq!(state.subscribers[&subscriber].pending_attach, None);
        assert!(matches!(
            receiver.try_recv(),
            Ok(DaemonMessage::Batch(batch)) if matches!(batch.events.as_slice(), [Event::Error { .. }])
        ));
        // A later attach attempt must be able to trigger OpenSession again.
        assert!(state.begin_attach(subscriber, "missing"));
    }

    #[test]
    fn delete_session_prunes_overlay_and_bounded_slow_subscriber_is_evicted() {
        let mut state = SharedState::new("test-epoch".to_string());
        let (slow_sender, _slow_receiver) = async_channel::bounded(1);
        let slow_id = state.subscribe(None, None, slow_sender);
        assert!(state.subscribers.contains_key(&slow_id));

        // First publish fills or overflows the 1-slot channel (Subscription already occupies slot 1).
        assert!(state.publish_envelope(&envelope(1, configured("s1"))).is_none());
        assert!(!state.subscribers.contains_key(&slow_id));
        assert!(state.sessions.contains_key("s1"));

        state.note_op(&Op::DeleteSession {
            session_id: "s1".to_string(),
        });
        assert!(!state.sessions.contains_key("s1"));
    }

    #[test]
    fn session_filter_restricts_session_batches_but_passes_global_events() {
        let mut state = SharedState::new("test-epoch".to_string());
        let (sender, receiver) = async_channel::bounded(16);
        state.subscribe(None, Some(vec!["s1".to_string()]), sender);
        let _ = receiver.try_recv();

        assert!(state.publish_envelope(&envelope(1, configured("s2"))).is_none());
        assert!(receiver.try_recv().is_err());

        assert!(state.publish_envelope(&envelope(2, configured("s1"))).is_none());
        assert!(matches!(
            receiver.try_recv(),
            Ok(DaemonMessage::Batch(batch)) if batch.session_id.as_deref() == Some("s1")
        ));

        assert!(
            state
                .publish_envelope(&envelope(
                    3,
                    Event::SessionList {
                        sessions: Vec::new(),
                    },
                ))
                .is_none()
        );
        assert!(matches!(
            receiver.try_recv(),
            Ok(DaemonMessage::Batch(batch)) if batch.session_id.is_none()
        ));
    }

    #[test]
    fn subscribers_receive_identical_ordered_batches() {
        let mut state = SharedState::new("test-epoch".to_string());
        let (first_sender, first) = async_channel::bounded(64);
        let (second_sender, second) = async_channel::bounded(64);
        state.subscribe(None, None, first_sender);
        state.subscribe(None, None, second_sender);
        assert!(
            state
                .publish_envelope(&envelope(1, configured("s1")))
                .is_none()
        );
        for receiver in [&first, &second] {
            assert!(matches!(
                receiver.try_recv(),
                Ok(DaemonMessage::Subscription { .. })
            ));
            let Ok(DaemonMessage::Batch(batch)) = receiver.try_recv() else {
                panic!("expected a projected batch");
            };
            assert_eq!(batch.sequence, 1);
        }

        let cursor = EventCursor {
            epoch: "test-epoch".to_string(),
            sequence: 0,
        };
        let (replay_sender, replay) = async_channel::bounded(64);
        state.subscribe(Some(cursor), None, replay_sender);
        assert!(matches!(
            replay.try_recv(),
            Ok(DaemonMessage::Subscription {
                resync_required: false,
                ..
            })
        ));
        let Ok(DaemonMessage::Batch(batch)) = replay.try_recv() else {
            panic!("a valid cursor replays retained events");
        };
        assert_eq!(batch.sequence, 1);
        assert!(replay.try_recv().is_err());
    }
}
