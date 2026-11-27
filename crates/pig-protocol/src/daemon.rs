use std::net::SocketAddr;

use super::*;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventCursor {
    pub epoch: String,
    pub sequence: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PendingApprovalRequest {
    pub request_id: String,
    pub tool: String,
    pub detail: String,
    #[serde(default)]
    pub danger_key: Option<String>,
    #[serde(default)]
    pub requested_write_roots: Vec<String>,
    #[serde(default)]
    pub available_decisions: Vec<ApprovalDecision>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PendingQuestionRequest {
    pub request_id: String,
    pub questions: Vec<QuestionItem>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionConfigurationSnapshot {
    pub cwd: PathBuf,
    pub model: String,
    pub provider_name: String,
    pub provider_id: Option<String>,
    pub model_id: Option<String>,
    pub reasoning_level: Option<String>,
    pub permissions: PermissionSettings,
    pub plan_enabled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextUsageSnapshot {
    pub used: u64,
    pub total: u64,
    #[serde(default)]
    pub cache_read_total: u64,
    #[serde(default)]
    pub input_total: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionStateSnapshot {
    pub session_id: String,
    pub configuration: Option<SessionConfigurationSnapshot>,
    pub items: Vec<ConversationItem>,
    pub pending_queue: Vec<QueueItem>,
    #[serde(default)]
    pub steer_queue: Vec<QueueItem>,
    pub pending_approvals: Vec<PendingApprovalRequest>,
    pub pending_questions: Vec<PendingQuestionRequest>,
    #[serde(default)]
    pub todos: Vec<TodoItem>,
    #[serde(default)]
    pub tasks: Vec<TaskSummary>,
    #[serde(default)]
    pub task_notifications: Vec<TaskNotification>,
    #[serde(default)]
    pub context_usage: Option<ContextUsageSnapshot>,
    #[serde(default)]
    pub context_breakdown: Option<ContextBreakdown>,
    pub running: bool,
    pub cursor: EventCursor,
}

pub const DAEMON_PROTOCOL_VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DaemonEndpoint {
    pub protocol: u32,
    pub address: SocketAddr,
    pub token: String,
    pub pid: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProjectedEventBatch {
    pub epoch: String,
    pub sequence: u64,
    pub session_id: Option<String>,
    pub events: Vec<Event>,
}

impl ProjectedEventBatch {
    pub fn cursor(&self) -> EventCursor {
        EventCursor {
            epoch: self.epoch.clone(),
            sequence: self.sequence,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DaemonRequest {
    Subscribe {
        after: Option<EventCursor>,
        #[serde(default)]
        session_ids: Option<Vec<String>>,
    },
    Op {
        op: Op,
    },
    Rpc {
        id: String,
        method: String,
        #[serde(default)]
        params: serde_json::Value,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DaemonMessage {
    Subscription {
        cursor: EventCursor,
        resync_required: bool,
    },
    Batch(ProjectedEventBatch),
    Snapshot(SessionStateSnapshot),
    RpcResult {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    PluginEvent {
        payload: String,
    },
    TerminalOutput {
        terminal_id: String,
        data: String,
    },
    TerminalExit {
        terminal_id: String,
    },
    Error {
        detail: String,
    },
}
