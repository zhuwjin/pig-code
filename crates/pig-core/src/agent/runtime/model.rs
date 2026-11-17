use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

pub const RUNTIME_SCHEMA_VERSION: u32 = 2;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentInitiator {
    User { request_id: Option<String> },
    Agent { agent_id: String, run_id: String },
    Goal { goal_id: String },
    Schedule { schedule_id: String },
    Workflow { workflow_id: String },
    Team { team_id: String },
    System,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeAgent {
    pub session_id: String,
    pub agent_id: String,
    pub parent_agent_id: Option<String>,
    pub depth: u32,
    pub profile: String,
    pub conversation_path: Option<PathBuf>,
    pub created_at_ms: u64,
    pub closed_at_ms: Option<u64>,
}

impl RuntimeAgent {
    pub fn root(session_id: String, profile: String, created_at_ms: u64) -> Self {
        Self {
            agent_id: session_id.clone(),
            session_id,
            parent_agent_id: None,
            depth: 0,
            profile,
            conversation_path: None,
            created_at_ms,
            closed_at_ms: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeRunStatus {
    Queued,
    Running,
    Stopping,
    Completed,
    Cancelled,
    Failed,
    Lost,
}

impl RuntimeRunStatus {
    pub fn is_active(self) -> bool {
        matches!(self, Self::Queued | Self::Running | Self::Stopping)
    }

    pub fn is_terminal(self) -> bool {
        !self.is_active()
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
            Self::Lost => "lost",
        }
    }

    pub fn from_str(value: &str) -> Option<Self> {
        Some(match value {
            "queued" => Self::Queued,
            "running" => Self::Running,
            "stopping" => Self::Stopping,
            "completed" => Self::Completed,
            "cancelled" => Self::Cancelled,
            "failed" => Self::Failed,
            "lost" => Self::Lost,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeRun {
    pub session_id: String,
    pub run_id: String,
    pub agent_id: String,
    pub backend_id: String,
    pub initiator: AgentInitiator,
    pub idempotency_key: Option<String>,
    pub causation_id: Option<String>,
    pub correlation_id: Option<String>,
    pub status: RuntimeRunStatus,
    pub stop_reason: Option<String>,
    pub usage: (u64, u64, u64),
    pub turns: usize,
    pub started_at_ms: u64,
    pub ended_at_ms: Option<u64>,
    pub deadline_at_ms: Option<u64>,
    pub idle_timeout_ms: Option<u64>,
    pub last_activity_at_ms: u64,
    pub watchdog_action: super::RunWatchdogAction,
    pub watchdog_alerts: std::collections::BTreeSet<super::RunWatchdogKind>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeConnectionState {
    NotApplicable,
    Connected,
    Disconnected,
    Reconciling,
}

impl RuntimeConnectionState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotApplicable => "not_applicable",
            Self::Connected => "connected",
            Self::Disconnected => "disconnected",
            Self::Reconciling => "reconciling",
        }
    }

    pub fn from_str(value: &str) -> Option<Self> {
        Some(match value {
            "not_applicable" => Self::NotApplicable,
            "connected" => Self::Connected,
            "disconnected" => Self::Disconnected,
            "reconciling" => Self::Reconciling,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutionAttempt {
    pub session_id: String,
    pub run_id: String,
    pub attempt: u64,
    pub backend_epoch: String,
    pub connection_state: RuntimeConnectionState,
    pub last_event_seq: u64,
    pub released_at_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MailboxMessageKind {
    Steer,
    Followup,
    Notification,
    Wake,
}

impl MailboxMessageKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Steer => "steer",
            Self::Followup => "followup",
            Self::Notification => "notification",
            Self::Wake => "wake",
        }
    }

    pub fn from_str(value: &str) -> Option<Self> {
        Some(match value {
            "steer" => Self::Steer,
            "followup" => Self::Followup,
            "notification" => Self::Notification,
            "wake" => Self::Wake,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MailboxDeliveryState {
    Queued,
    Claimed,
    Consumed,
}

impl MailboxDeliveryState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Claimed => "claimed",
            Self::Consumed => "consumed",
        }
    }

    pub fn from_str(value: &str) -> Option<Self> {
        Some(match value {
            "queued" => Self::Queued,
            "claimed" => Self::Claimed,
            "consumed" => Self::Consumed,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RuntimeMailboxPayload {
    pub text: String,
    pub notification: Option<pig_protocol::TaskNotification>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeMailboxMessage {
    pub session_id: String,
    pub message_id: String,
    pub target_agent_id: String,
    pub target_run_id: Option<String>,
    pub kind: MailboxMessageKind,
    pub payload: serde_json::Value,
    pub notification_id: Option<String>,
    pub state: MailboxDeliveryState,
    pub created_at_ms: u64,
    pub claimed_at_ms: Option<u64>,
    pub consumed_at_ms: Option<u64>,
}

impl RuntimeMailboxMessage {
    pub fn new(
        session_id: String,
        message_id: String,
        target_agent_id: String,
        target_run_id: Option<String>,
        kind: MailboxMessageKind,
        payload: serde_json::Value,
        notification_id: Option<String>,
    ) -> Self {
        Self {
            session_id,
            message_id,
            target_agent_id,
            target_run_id,
            kind,
            payload,
            notification_id,
            state: MailboxDeliveryState::Queued,
            created_at_ms: crate::model_io::now_ms(),
            claimed_at_ms: None,
            consumed_at_ms: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeEventKind {
    AgentRegistered,
    RunStarted,
    RunStopping,
    RunWatchdog,
    RunSettled,
    MailboxEnqueued,
    MailboxClaimed,
    MailboxConsumed,
    ExecutionReleased,
    ConnectionChanged,
    AttemptStarted,
}

impl RuntimeEventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AgentRegistered => "agent_registered",
            Self::RunStarted => "run_started",
            Self::RunStopping => "run_stopping",
            Self::RunWatchdog => "run_watchdog",
            Self::RunSettled => "run_settled",
            Self::MailboxEnqueued => "mailbox_enqueued",
            Self::MailboxClaimed => "mailbox_claimed",
            Self::MailboxConsumed => "mailbox_consumed",
            Self::ExecutionReleased => "execution_released",
            Self::ConnectionChanged => "connection_changed",
            Self::AttemptStarted => "attempt_started",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeControlEvent {
    pub session_id: String,
    pub sequence: u64,
    pub event_id: String,
    pub kind: RuntimeEventKind,
    pub agent_id: Option<String>,
    pub run_id: Option<String>,
    pub attempt: Option<u64>,
    pub backend_epoch: Option<String>,
    pub payload: serde_json::Value,
    pub created_at_ms: u64,
}

impl RuntimeControlEvent {
    pub fn protocol(&self) -> pig_protocol::AgentControlEvent {
        pig_protocol::AgentControlEvent {
            session_id: self.session_id.clone(),
            sequence: self.sequence,
            event_id: self.event_id.clone(),
            kind: self.kind.as_str().to_string(),
            agent_id: self.agent_id.clone(),
            run_id: self.run_id.clone(),
            attempt: self.attempt,
            backend_epoch: self.backend_epoch.clone(),
            payload: self.payload.clone(),
            created_at_ms: self.created_at_ms,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct RuntimeMailboxDeliveryReceipt {
    pub message_ids: std::collections::BTreeSet<String>,
    pub notification_ids: std::collections::BTreeSet<String>,
}

#[derive(Clone, Debug, Default)]
pub struct RuntimeSnapshot {
    pub agents: Vec<RuntimeAgent>,
    pub runs: Vec<RuntimeRun>,
    pub attempts: Vec<ExecutionAttempt>,
    pub mailbox: Vec<RuntimeMailboxMessage>,
    pub latest_sequence: u64,
}

#[derive(Debug)]
pub enum RuntimeError {
    Closed,
    NotFound(String),
    Conflict(String),
    InvalidTransition(String),
    StaleAttempt {
        run_id: String,
        attempt: u64,
        backend_epoch: String,
    },
    Store(String),
    Backend(String),
    ChannelClosed,
}

impl std::fmt::Display for RuntimeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Closed => write!(formatter, "agent runtime is closed"),
            Self::NotFound(detail) | Self::Conflict(detail) | Self::InvalidTransition(detail) => {
                formatter.write_str(detail)
            }
            Self::StaleAttempt {
                run_id,
                attempt,
                backend_epoch,
            } => write!(
                formatter,
                "stale execution attempt for {run_id}: attempt {attempt}, epoch {backend_epoch}"
            ),
            Self::Store(detail) => write!(formatter, "runtime store failed: {detail}"),
            Self::Backend(detail) => write!(formatter, "agent backend preflight failed: {detail}"),
            Self::ChannelClosed => formatter.write_str("runtime command channel closed"),
        }
    }
}

impl std::error::Error for RuntimeError {}

pub fn new_runtime_id(prefix: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{timestamp}-{}-{sequence}", std::process::id())
}
