use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use fs2::FileExt as _;
use futures_util::{SinkExt as _, StreamExt as _};
use pig_core::{AgentHandle, Projected, ProjectionRegistry, ProjectionResync};
use pig_protocol::{
    Event, EventCursor, EventEnvelope, Op, PendingApprovalRequest, PendingQuestionRequest,
    SessionStateSnapshot,
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
}

impl SessionOverlay {
    fn apply(&mut self, event: &Event) {
        match event {
            Event::QueueChanged { items, .. } => self.pending_queue = items.clone(),
            Event::SteerChanged { items, .. } => self.steer_queue = items.clone(),
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
}

struct SharedState {
    epoch: String,
    latest: u64,
    history: VecDeque<ProjectedEventBatch>,
    projection: ProjectionRegistry,
    sessions: HashMap<String, SessionOverlay>,
    loading_sessions: HashSet<String>,
    new_sessions: usize,
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
            new_sessions: 0,
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
        sender: async_channel::Sender<DaemonMessage>,
    ) -> u64 {
        self.next_subscriber += 1;
        let id = self.next_subscriber;
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
                .filter(|batch| batch.sequence > sequence)
                .cloned()
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let _ = sender.try_send(DaemonMessage::Subscription {
            cursor: start,
            resync_required,
        });
        for batch in replay {
            let _ = sender.try_send(DaemonMessage::Batch(batch));
        }
        self.subscribers.insert(
            id,
            Subscriber {
                sender,
                pending_attach: None,
            },
        );
        id
    }

    fn unsubscribe(&mut self, id: u64) {
        self.subscribers.remove(&id);
    }

    fn note_op(&mut self, op: &Op) {
        if matches!(op, Op::NewSession { .. }) {
            self.new_sessions = self.new_sessions.saturating_add(1);
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
            running: self.projection.session_running(session_id),
            cursor: self.cursor(),
        }
    }

    fn begin_attach(&mut self, subscriber_id: u64, session_id: &str) -> bool {
        if self
            .sessions
            .get(session_id)
            .is_some_and(|session| session.ready)
        {
            let snapshot = self.snapshot(session_id);
            if let Some(subscriber) = self.subscribers.get_mut(&subscriber_id) {
                subscriber.pending_attach = None;
                let _ = subscriber
                    .sender
                    .try_send(DaemonMessage::Snapshot(snapshot));
            }
            return false;
        }
        if let Some(subscriber) = self.subscribers.get_mut(&subscriber_id) {
            subscriber.pending_attach = Some(session_id.to_string());
        }
        self.loading_sessions.insert(session_id.to_string())
    }

    fn apply_session_event(&mut self, event: &Event) {
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
            let ready = self.new_sessions > 0;
            self.new_sessions = self.new_sessions.saturating_sub(1);
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
            *overlay = SessionOverlay {
                ready,
                configuration: Some(configuration),
                ..SessionOverlay::default()
            };
            if !ready {
                self.loading_sessions.insert(session_id.to_string());
            }
            return;
        }
        let overlay = self.sessions.entry(session_id.to_string()).or_default();
        overlay.apply(event);
        if matches!(event, Event::SessionReplayComplete { .. }) {
            overlay.ready = true;
            self.loading_sessions.remove(session_id);
        }
    }

    fn publish_envelope(&mut self, envelope: &EventEnvelope) -> Option<ProjectionResync> {
        if envelope.sequence <= self.latest {
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
        self.apply_session_event(&envelope.event);
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
                && !is_error
            {
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
            self.apply_session_event(&envelope.event);
        }
        let cursor = self.cursor();
        self.subscribers.retain(|_, subscriber| {
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

fn write_endpoint(data_dir: &Path, endpoint: &DaemonEndpoint) -> Result<EndpointGuard, String> {
    let directory = daemon_directory(data_dir);
    std::fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| error.to_string())?;
    }
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
    std::fs::rename(&temporary, &path).map_err(|error| error.to_string())?;
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
    loop {
        let (stream, _) = listener.accept().await.map_err(|error| error.to_string())?;
        let agent = agent.clone();
        let state = state.clone();
        let token = endpoint.token.clone();
        tokio::spawn(async move {
            if let Err(error) = handle_connection(stream, agent, state, token).await {
                tracing::debug!(%error, "daemon client disconnected");
            }
        });
    }
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
                let result = state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .reset_with_snapshot(snapshot);
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
    let first = tokio::time::timeout(std::time::Duration::from_secs(10), websocket.next())
        .await
        .map_err(|_| "timed out waiting for daemon subscription".to_string())?
        .ok_or("daemon connection closed before subscription")?
        .map_err(|error| error.to_string())?;
    let DaemonRequest::Subscribe { after } = parse_request(first)? else {
        return Err("daemon subscription must be the first request".to_string());
    };
    let (mut writer, mut reader) = websocket.split();
    let (outgoing, receiver) = async_channel::unbounded::<DaemonMessage>();
    let subscriber_id = state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .subscribe(after, outgoing);
    let write_task = tokio::spawn(async move {
        while let Ok(message) = receiver.recv().await {
            let encoded = serde_json::to_string(&message).map_err(|error| error.to_string())?;
            writer
                .send(Message::text(encoded))
                .await
                .map_err(|error| error.to_string())?;
        }
        Ok::<(), String>(())
    });
    while let Some(message) = reader.next().await {
        let message = message.map_err(|error| error.to_string())?;
        let DaemonRequest::Op { op } = parse_request(message)? else {
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
                        detail: "daemon shutdown is not tied to a client".to_string(),
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
        if should_forward {
            agent
                .ops
                .send(op)
                .await
                .map_err(|error| error.to_string())?;
        }
    }
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .unsubscribe(subscriber_id);
    write_task.abort();
    Ok(())
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
        let (sender, receiver) = async_channel::unbounded();
        let subscriber = state.subscribe(None, sender);
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
        assert_eq!(snapshot.cursor.sequence, 4);
        assert!(snapshot.configuration.is_some());
        assert!(receiver.try_recv().is_err());

        let (second_sender, second_receiver) = async_channel::unbounded();
        let second = state.subscribe(None, second_sender);
        assert!(!state.begin_attach(second, "s1"));
        assert!(matches!(
            second_receiver.try_recv(),
            Ok(DaemonMessage::Subscription { .. })
        ));
        let Ok(DaemonMessage::Snapshot(second_snapshot)) = second_receiver.try_recv() else {
            panic!("ready sessions attach directly from a snapshot");
        };
        assert_eq!(second_snapshot.cursor.sequence, 4);
        assert_eq!(second_snapshot.items.len(), 1);

        assert!(
            state
                .publish_envelope(&envelope(
                    5,
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
            assert_eq!(batch.sequence, 5);
            assert!(matches!(
                batch.events.as_slice(),
                [Event::ApprovalResolved { request_id, .. }] if request_id == "approval-1"
            ));
        }
        assert!(state.snapshot("s1").pending_approvals.is_empty());
    }

    #[test]
    fn subscribers_receive_identical_ordered_batches() {
        let mut state = SharedState::new("test-epoch".to_string());
        let (first_sender, first) = async_channel::unbounded();
        let (second_sender, second) = async_channel::unbounded();
        state.subscribe(None, first_sender);
        state.subscribe(None, second_sender);
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
        let (replay_sender, replay) = async_channel::unbounded();
        state.subscribe(Some(cursor), replay_sender);
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
