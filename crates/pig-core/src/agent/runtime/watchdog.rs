use serde::{Deserialize, Serialize};

use super::model::{RuntimeRun, RuntimeRunStatus};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunWatchdogAction {
    #[default]
    Observe,
    Stop,
}

impl RunWatchdogAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Observe => "observe",
            Self::Stop => "stop",
        }
    }

    pub fn from_str(value: &str) -> Option<Self> {
        match value {
            "observe" => Some(Self::Observe),
            "stop" => Some(Self::Stop),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RunWatchdogPolicy {
    pub deadline: Option<std::time::Duration>,
    pub idle_timeout: Option<std::time::Duration>,
    pub action: RunWatchdogAction,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunWatchdogKind {
    Deadline,
    Idle,
}

impl RunWatchdogKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Deadline => "deadline",
            Self::Idle => "idle",
        }
    }

    pub fn stop_reason(self) -> &'static str {
        match self {
            Self::Deadline => "run deadline exceeded",
            Self::Idle => "run idle timeout exceeded",
        }
    }
}

pub(crate) fn due(run: &RuntimeRun, now_ms: u64) -> Vec<RunWatchdogKind> {
    if !run.status.is_active() {
        return Vec::new();
    }
    let mut alerts = Vec::new();
    if run
        .deadline_at_ms
        .is_some_and(|deadline| now_ms >= deadline)
    {
        alerts.push(RunWatchdogKind::Deadline);
    }
    if run.status == RuntimeRunStatus::Running
        && run
            .idle_timeout_ms
            .is_some_and(|timeout| now_ms >= run.last_activity_at_ms.saturating_add(timeout))
    {
        alerts.push(RunWatchdogKind::Idle);
    }
    alerts
}

pub(crate) fn duration_ms(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::runtime::AgentInitiator;

    fn run(status: RuntimeRunStatus) -> RuntimeRun {
        RuntimeRun {
            session_id: "s1".into(),
            run_id: "r1".into(),
            agent_id: "a1".into(),
            backend_id: "local".into(),
            initiator: AgentInitiator::System,
            idempotency_key: None,
            causation_id: None,
            correlation_id: None,
            status,
            stop_reason: None,
            usage: (0, 0, 0),
            turns: 0,
            started_at_ms: 10,
            ended_at_ms: None,
            deadline_at_ms: Some(30),
            idle_timeout_ms: Some(10),
            last_activity_at_ms: 20,
            watchdog_action: RunWatchdogAction::Observe,
            watchdog_alerts: Default::default(),
        }
    }

    #[test]
    fn deadlines_include_queueing_but_idle_only_applies_to_running_runs() {
        assert_eq!(due(&run(RuntimeRunStatus::Queued), 29), []);
        assert_eq!(
            due(&run(RuntimeRunStatus::Queued), 30),
            [RunWatchdogKind::Deadline]
        );
        assert_eq!(due(&run(RuntimeRunStatus::Running), 29), []);
        assert_eq!(
            due(&run(RuntimeRunStatus::Running), 30),
            [RunWatchdogKind::Deadline, RunWatchdogKind::Idle]
        );
        assert_eq!(due(&run(RuntimeRunStatus::Completed), 100), []);
    }
}
