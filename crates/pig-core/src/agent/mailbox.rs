use std::collections::VecDeque;

use pig_protocol::TaskNotification;

#[derive(Clone, Debug)]
pub(crate) struct AgentSteer {
    pub id: String,
    pub text: String,
    pub notification: Option<TaskNotification>,
}

impl AgentSteer {
    pub(crate) fn message(text: String) -> Self {
        Self {
            id: next_steer_id(),
            text,
            notification: None,
        }
    }

    pub(crate) fn notification(notification: TaskNotification, text: String) -> Self {
        Self {
            id: next_steer_id(),
            text,
            notification: Some(notification),
        }
    }

    pub(crate) fn restored(
        id: String,
        text: String,
        notification: Option<TaskNotification>,
    ) -> Self {
        Self {
            id,
            text,
            notification,
        }
    }
}

fn next_steer_id() -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("steer-{timestamp}-{sequence}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn releasing_notifications_keeps_plain_steers_pending() {
        let notification = pig_protocol::TaskNotification {
            id: "task:s1:b1:completed".into(),
            task_id: "b1".into(),
            kind: pig_protocol::TaskKind::Subagent,
            status: pig_protocol::TaskStatus::Completed,
            exit_code: Some(0),
            severity: pig_protocol::NotificationSeverity::Info,
            description: "done".into(),
            title: "done".into(),
            body: "done".into(),
            duration_ms: None,
            turns: None,
            model: None,
            agent_id: None,
            record_path: None,
            output: None,
        };
        let mut mailbox = AgentMailbox::default();
        mailbox.push(AgentSteer::message("later".into()));
        mailbox.push(AgentSteer::notification(notification, "done".into()));
        assert_eq!(mailbox.drain_notifications().len(), 1);
        assert_eq!(mailbox.drain()[0].text, "later");
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct AgentMailbox {
    pending: VecDeque<AgentSteer>,
}

impl AgentMailbox {
    pub(super) fn push(&mut self, steer: AgentSteer) {
        self.pending.push_back(steer);
    }

    pub(super) fn prepend(&mut self, steer: AgentSteer) {
        self.pending.push_front(steer);
    }

    pub(super) fn drain(&mut self) -> Vec<AgentSteer> {
        self.pending.drain(..).collect()
    }

    pub(super) fn retain(&mut self, keep: impl FnMut(&AgentSteer) -> bool) {
        self.pending.retain(keep);
    }

    pub(super) fn restore(&mut self, steer: AgentSteer) {
        if self.pending.iter().any(|pending| pending.id == steer.id) {
            return;
        }
        self.pending.push_back(steer);
    }

    pub(super) fn drain_notifications(&mut self) -> Vec<AgentSteer> {
        let mut notifications = Vec::new();
        let mut pending = VecDeque::new();
        for steer in self.pending.drain(..) {
            if steer.notification.is_some() {
                notifications.push(steer);
            } else {
                pending.push_back(steer);
            }
        }
        self.pending = pending;
        notifications
    }
}
