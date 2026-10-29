use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    Shell,
    Subagent,
}

impl TaskKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Shell => "shell",
            Self::Subagent => "subagent",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Queued,
    Running,
    Completed,
    Failed,
    TimedOut,
    Killed,
    Lost,
}

impl TaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::TimedOut => "timed_out",
            Self::Killed => "killed",
            Self::Lost => "lost",
        }
    }

    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Queued | Self::Running)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskSummary {
    pub id: String,
    pub kind: TaskKind,
    pub command: String,
    pub status: TaskStatus,
    pub exit_code: Option<i32>,
    pub reason: Option<String>,
    pub started_at_ms: u64,
    pub ended_at_ms: Option<u64>,
    pub output_tail: String,
    pub agent_id: Option<String>,
    pub spill_path: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationSeverity {
    Info,
    Warning,
}

impl NotificationSeverity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TaskNotificationOutput {
    File {
        path: PathBuf,
        bytes: u64,
    },
    Preview {
        text: String,
        total_bytes: u64,
        truncated: bool,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskNotification {
    pub id: String,
    pub task_id: String,
    pub kind: TaskKind,
    pub status: TaskStatus,
    pub exit_code: Option<i32>,
    pub severity: NotificationSeverity,
    pub description: String,
    pub title: String,
    pub body: String,
    pub duration_ms: Option<u64>,
    pub turns: Option<u32>,
    pub model: Option<String>,
    pub agent_id: Option<String>,
    pub record_path: Option<PathBuf>,
    pub output: Option<TaskNotificationOutput>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryChannel {
    Automatic,
    TaskWait,
}

impl DeliveryChannel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Automatic => "automatic",
            Self::TaskWait => "task_wait",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryState {
    Scheduled,
    Delivered,
}

impl DeliveryState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Scheduled => "scheduled",
            Self::Delivered => "delivered",
        }
    }
}
